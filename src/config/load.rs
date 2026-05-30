use std::{fs, io, path::Path};

use anyhow::{Result, anyhow};

use crate::{config::schema::RawDecuneConfig, error::ResultExt};

const SUPPORTED_CONFIG_VERSION: u32 = 1;

pub(crate) fn load_config_file(path: impl AsRef<Path>) -> Result<RawDecuneConfig> {
    let path = path.as_ref();
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RawDecuneConfig::empty());
        }
        Err(error) => {
            return Err(error).with_path_context("read decune config file", path);
        }
    };

    parse_config_str(&contents, path)
}

fn parse_config_str(contents: &str, path: &Path) -> Result<RawDecuneConfig> {
    let config = toml::from_str::<RawDecuneConfig>(contents)
        .with_path_context("parse decune config file", path)?;

    validate_version(&config, path)?;
    Ok(config)
}

fn validate_version(config: &RawDecuneConfig, path: &Path) -> Result<()> {
    match config.version {
        Some(SUPPORTED_CONFIG_VERSION) => Ok(()),
        Some(version) => Err(anyhow!(
            "Unsupported decune config version {version} in {}; expected version {SUPPORTED_CONFIG_VERSION}",
            path.display()
        )),
        None => Err(anyhow!(
            "Missing required decune config version in {}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::schema::{
        RawCommand, RawDotfileConflict, RawGitHttpsMode, RawGithubCredentialsMode, RawHookLocation,
        RawMountType, RawOnAutoForward, RawPortProtocol, RawSshAgentMode,
    };

    use super::*;

    fn config_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("decune-config-load-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_file_loads_empty_config() {
        let path = config_path("missing");
        let _ = fs::remove_file(&path);

        let config = load_config_file(&path).unwrap();

        assert_eq!(config, RawDecuneConfig::empty());
    }

    #[test]
    fn existing_empty_file_requires_version() {
        let path = config_path("empty");
        fs::write(&path, "").unwrap();

        let error = load_config_file(&path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Missing required decune config version")
        );
        assert!(error.to_string().contains(&path.display().to_string()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let path = config_path("version");
        fs::write(&path, "version = 2\n").unwrap();

        let error = load_config_file(&path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Unsupported decune config version 2")
        );
        assert!(error.to_string().contains(&path.display().to_string()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn parse_error_includes_path() {
        let path = config_path("parse-error");
        fs::write(&path, "version = 1\nshelll = '/bin/zsh'\n").unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains(&path.display().to_string()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn top_level_typo_key_is_rejected_with_key_and_path() {
        let path = config_path("top-level-typo");
        fs::write(&path, "version = 1\nshelll = '/bin/zsh'\n").unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains("shelll"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn spec_example_is_loaded() {
        let path = config_path("spec-example");
        fs::write(
            &path,
            r#"
version = 1
shell = "/bin/zsh"

[features."ghcr.io/devcontainers/features/github-cli:1"]
version = "latest"

[features."ghcr.io/duduribeiro/devcontainer-features/neovim:1"]
version = "nightly"

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_only = true
resolve_symlink = true
on_conflict = "replace-symlink"

[[mounts]]
source = "~/work"
target = "/workspaces/work"
type = "bind"
read_only = false
resolve_symlink = true
create = false

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
protocol = "tcp"
require_local = false
label = "web"

[ports.auto]
enabled = true
min = 1024
max = 32768
ignore = [22, 2375, 2376]
on_auto_forward = "notify"

[credentials.git]
enabled = true
copy_user = true
copy_global_config = false
https = "host-helper"
ssh_agent = "auto"

[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true

[[hooks.before_post_create]]
command = "scripts/before-post-create.sh"
where = "container"
user = "remote"
shell = true

[[hooks.after_post_start]]
command = ["bash", "scripts/after-start.sh"]
where = "container"
user = "remote"
shell = false
"#,
        )
        .unwrap();

        let config = load_config_file(&path).unwrap();

        assert_eq!(config.version, Some(1));
        assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(config.features.len(), 2);
        assert_eq!(
            config
                .features
                .get("ghcr.io/devcontainers/features/github-cli:1")
                .unwrap()
                .options
                .get("version")
                .and_then(|value| value.as_str()),
            Some("latest")
        );
        assert_eq!(config.dotfiles.len(), 1);
        assert_eq!(config.dotfiles[0].source.as_deref(), Some("~/.config/nvim"));
        assert_eq!(config.dotfiles[0].target, ".config/nvim");
        assert_eq!(config.dotfiles[0].read_only, Some(true));
        assert_eq!(config.dotfiles[0].resolve_symlink, Some(true));
        assert_eq!(
            config.dotfiles[0].on_conflict,
            Some(RawDotfileConflict::ReplaceSymlink)
        );
        assert_eq!(config.mounts.len(), 1);
        assert_eq!(config.mounts[0].source.as_deref(), Some("~/work"));
        assert_eq!(config.mounts[0].target, "/workspaces/work");
        assert_eq!(config.mounts[0].mount_type, Some(RawMountType::Bind));
        assert_eq!(config.mounts[0].read_only, Some(false));
        assert_eq!(config.mounts[0].resolve_symlink, Some(true));
        assert_eq!(config.mounts[0].create, None);
        assert_eq!(config.ports.entries.len(), 1);
        assert_eq!(config.ports.entries[0].container, 3000);
        assert_eq!(config.ports.entries[0].host, Some(3000));
        assert_eq!(
            config.ports.entries[0].host_ip.as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(config.ports.entries[0].protocol, Some(RawPortProtocol::Tcp));
        assert_eq!(config.ports.entries[0].require_local, Some(false));
        assert_eq!(config.ports.entries[0].label.as_deref(), Some("web"));
        let auto = config.ports.auto.unwrap();
        assert_eq!(auto.enabled, Some(true));
        assert_eq!(auto.min, Some(1024));
        assert_eq!(auto.max, Some(32768));
        assert_eq!(auto.ignore, Some(vec![22, 2375, 2376]));
        assert_eq!(auto.on_auto_forward, Some(RawOnAutoForward::Notify));
        let git = config.credentials.git.unwrap();
        assert_eq!(git.enabled, Some(true));
        assert_eq!(git.copy_user, Some(true));
        assert_eq!(git.copy_global_config, Some(false));
        assert_eq!(git.https, Some(RawGitHttpsMode::HostHelper));
        assert_eq!(git.ssh_agent, Some(RawSshAgentMode::Auto));
        let github = config.credentials.github.unwrap();
        assert_eq!(github.enabled, Some(true));
        assert_eq!(github.mode, Some(RawGithubCredentialsMode::GhTokenFile));
        assert_eq!(github.install_feature_if_missing, Some(true));
        assert_eq!(config.hooks.before_post_create.len(), 1);
        assert_eq!(
            config.hooks.before_post_create[0].command,
            RawCommand::Shell("scripts/before-post-create.sh".to_owned())
        );
        assert_eq!(
            config.hooks.before_post_create[0].location,
            Some(RawHookLocation::Container)
        );
        assert_eq!(
            config.hooks.before_post_create[0].user.as_deref(),
            Some("remote")
        );
        assert_eq!(config.hooks.before_post_create[0].shell, Some(true));
        assert_eq!(config.hooks.after_post_start.len(), 1);
        assert_eq!(
            config.hooks.after_post_start[0].command,
            RawCommand::Args(vec!["bash".to_owned(), "scripts/after-start.sh".to_owned()])
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabled_dotfile_requires_source() {
        let path = config_path("dotfile-requires-source");
        fs::write(
            &path,
            r#"
version = 1

[[dotfiles]]
target = ".config/nvim"
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("source"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabled_mount_requires_type() {
        let path = config_path("mount-requires-type");
        fs::write(
            &path,
            r#"
version = 1

[[mounts]]
source = "/tmp/work"
target = "/work"
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("type"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabled_bind_mount_requires_source() {
        let path = config_path("bind-mount-requires-source");
        fs::write(
            &path,
            r#"
version = 1

[[mounts]]
target = "/work"
type = "bind"
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("source"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn nested_typo_key_is_rejected() {
        let path = config_path("nested-typo");
        fs::write(
            &path,
            r#"
version = 1

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_olny = true
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("read_olny"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn nested_credentials_typo_key_is_rejected() {
        let path = config_path("nested-credentials-typo");
        fs::write(
            &path,
            r#"
version = 1

[credentials.git]
copy_usr = true
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("copy_usr"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn nested_hook_typo_key_is_rejected() {
        let path = config_path("nested-hook-typo");
        fs::write(
            &path,
            r#"
version = 1

[[hooks.before_post_create]]
command = "setup.sh"
wrkdir = "/work"
"#,
        )
        .unwrap();

        let error = load_config_file(&path).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to parse decune config file"));
        assert!(message.contains("wrkdir"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn global_and_project_paths_are_loaded_independently() {
        let global_path = config_path("global");
        let project_path = config_path("project");
        fs::write(&global_path, "version = 1\nshell = '/bin/bash'\n").unwrap();
        fs::write(&project_path, "version = 1\nshell = '/bin/zsh'\n").unwrap();

        let global = load_config_file(&global_path).unwrap();
        let project = load_config_file(&project_path).unwrap();

        assert_eq!(global.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(project.shell.as_deref(), Some("/bin/zsh"));

        let _ = fs::remove_file(global_path);
        let _ = fs::remove_file(project_path);
    }
}

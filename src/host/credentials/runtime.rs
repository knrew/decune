use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::resolved::ResolvedConfig,
    docker::{
        exec::{ExecCommandSpec, exec_capture},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
    ui,
};

pub(crate) const DECUNE_RUNTIME_TARGET: &str = "/run/decune";
pub(super) const GIT_CREDENTIAL_HELPER_NAME: &str = "git-credential-decune";
pub(super) const HOST_GITCONFIG_NAME: &str = "host-gitconfig";
pub(super) const GITHUB_CLI_LEGACY_TOKEN_DIR_NAME: &str = "gh-token";
pub(super) const GITHUB_CLI_LEGACY_TOKEN_FILE_NAME: &str = "token";
pub(super) const GITHUB_CLI_SECRET_DIR_NAME: &str = "secrets";
pub(super) const GITHUB_CLI_TOKEN_FILE_NAME: &str = "github-token";
pub(super) const HOST_DAEMON_SOCKET_TARGET: &str = "/run/decune/host-daemon.sock";
pub(super) const GIT_CREDENTIAL_HELPER_TARGET: &str = "/run/decune/git-credential-decune";
pub(super) const HOST_GITCONFIG_TARGET: &str = "/run/decune/host-gitconfig";
pub(crate) const GITHUB_CLI_LEGACY_TOKEN_DIR_TARGET: &str = "/run/decune/gh-token";
pub(crate) const GITHUB_CLI_TOKEN_TARGET: &str = "/run/decune/secrets/github-token";
pub(crate) const GITHUB_CLI_CONFIG_TARGET: &str = "/run/decune/gh";
pub(crate) const SSH_AGENT_SOCKET_TARGET: &str = "/run/decune/ssh-agent.sock";

pub(crate) struct GitCredentialRuntime {
    pub(super) mounts: Vec<DockerMountSpec>,
    pub(super) cleanup_paths: Vec<PathBuf>,
}

impl GitCredentialRuntime {
    pub(crate) fn empty() -> Self {
        Self {
            mounts: Vec::new(),
            cleanup_paths: Vec::new(),
        }
    }

    pub(crate) fn mounts(&self) -> &[DockerMountSpec] {
        &self.mounts
    }
}

impl Drop for GitCredentialRuntime {
    fn drop(&mut self) {
        for path in &self.cleanup_paths {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
pub(crate) struct GithubCliRuntime {
    pub(super) mounts: Vec<DockerMountSpec>,
    pub(super) container_env: BTreeMap<String, String>,
    pub(super) token_file: Option<PathBuf>,
}

impl GithubCliRuntime {
    pub(crate) fn empty() -> Self {
        Self {
            mounts: Vec::new(),
            container_env: BTreeMap::new(),
            token_file: None,
        }
    }

    pub(crate) fn mounts(&self) -> &[DockerMountSpec] {
        &self.mounts
    }

    pub(crate) fn container_env(&self) -> &BTreeMap<String, String> {
        &self.container_env
    }

    #[cfg(test)]
    pub(crate) fn token_file(&self) -> Option<&Path> {
        self.token_file.as_deref()
    }
}

impl Drop for GithubCliRuntime {
    fn drop(&mut self) {
        if let Some(path) = &self.token_file {
            scrub_github_cli_token_file_best_effort(path);
        }
    }
}

pub(super) fn scrub_github_cli_token_file_best_effort(path: &Path) {
    match fs::OpenOptions::new().write(true).truncate(true).open(path) {
        Ok(file) => {
            if let Err(error) = file.sync_all() {
                ui::warn(&format!(
                    "Failed to scrub GitHub CLI token file: {}. Remove it manually: {error}",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => ui::warn(&format!(
            "Failed to scrub GitHub CLI token file: {}. Remove it manually: {error}",
            path.display()
        )),
    }
}

pub(super) fn cleanup_github_cli_token_file_best_effort(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => ui::warn(&format!(
            "Failed to remove GitHub CLI token file: {}. Remove it manually: {error}",
            path.display()
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SshAgentRuntime {
    pub(super) mounts: Vec<DockerMountSpec>,
    pub(super) container_env: BTreeMap<String, String>,
}

impl SshAgentRuntime {
    pub(crate) fn empty() -> Self {
        Self {
            mounts: Vec::new(),
            container_env: BTreeMap::new(),
        }
    }

    pub(crate) fn mounts(&self) -> &[DockerMountSpec] {
        &self.mounts
    }

    pub(crate) fn container_env(&self) -> &BTreeMap<String, String> {
        &self.container_env
    }
}

pub(crate) async fn install_staged_host_gitconfig(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !config.credentials.git.enabled || !config.credentials.git.copy_global_config {
        return Ok(());
    }

    let Some(remote_home) = remote_user.home.as_deref() else {
        ui::warn(&format!(
            "Git global config copy is unavailable in container: {container}: remote user home is unavailable"
        ));
        return Ok(());
    };

    let target = format!("{remote_home}/.gitconfig");
    let script = format!(
        "set -e\nif [ -f {source} ]; then rm -f {target}; cp {source} {target}; chown {uid}:{gid} {target}; chmod 600 {target}; fi\n",
        source = shell_quote(HOST_GITCONFIG_TARGET),
        target = shell_quote(&target),
        uid = remote_user.uid,
        gid = remote_user.gid,
    );

    let setup_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to install host Git config in container: {container}"));
    if setup_result.is_err() {
        ui::warn(&format!(
            "Host Git config copy is unavailable in container: {container}"
        ));
    }

    Ok(())
}

pub(crate) fn remove_staged_host_gitconfig(runtime_dir: &Path) -> Result<()> {
    let path = runtime_dir.join(HOST_GITCONFIG_NAME);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove staged host Git config: {}",
                path.display()
            )
        }),
    }
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

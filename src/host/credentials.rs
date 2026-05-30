use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedGitCredentials, ResolvedGithubCredentials},
        types::{GitHttpsMode, GithubCredentialsMode, MountType, SshAgentMode},
    },
    docker::{
        exec::{ExecCommandSpec, exec_capture, exec_capture_output},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
    ui,
};

pub(crate) const DECUNE_RUNTIME_TARGET: &str = "/run/decune";
const GIT_CREDENTIAL_HELPER_NAME: &str = "git-credential-decune";
const GIT_CREDENTIAL_HELPER_LINUX_X86_64_NAME: &str = "git-credential-decune-linux-x86_64";
const HOST_GITCONFIG_NAME: &str = "host-gitconfig";
const GITHUB_CLI_TOKEN_DIR_NAME: &str = "gh-token";
const GITHUB_CLI_TOKEN_FILE_NAME: &str = "token";
const HOST_DAEMON_SOCKET_TARGET: &str = "/run/decune/host-daemon.sock";
const GIT_CREDENTIAL_HELPER_TARGET: &str = "/run/decune/git-credential-decune";
pub(crate) const GITHUB_CLI_TOKEN_DIR_TARGET: &str = "/run/decune/gh-token";
pub(crate) const GITHUB_CLI_TOKEN_TARGET: &str = "/run/decune/gh-token/token";
pub(crate) const GITHUB_CLI_CONFIG_TARGET: &str = "/run/decune/gh";
const GITHUB_CLI_CONFIG_TOKEN_TARGET: &str = "/run/decune/gh/.decune-token";
pub(crate) const SSH_AGENT_SOCKET_TARGET: &str = "/run/decune/ssh-agent.sock";
const REQUEST_TYPE_CREDENTIAL: &str = "credential";
// host の decune binary は container OS/libc と一致しないため，Linux static helper を展開する．
const GIT_CREDENTIAL_HELPER_LINUX_X86_64: &[u8] =
    include_bytes!("assets/git-credential-decune-linux-x86_64");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GitCredentialAction {
    Get,
    Store,
    Erase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitCredentialCommand {
    Fill,
    Approve,
    Reject,
}

impl GitCredentialCommand {
    fn as_git_arg(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }

    pub(crate) fn from_action(action: GitCredentialAction) -> Self {
        match action {
            GitCredentialAction::Get => Self::Fill,
            GitCredentialAction::Store => Self::Approve,
            GitCredentialAction::Erase => Self::Reject,
        }
    }
}

pub(crate) trait GitCredentialExecutor: Send + Sync {
    fn run(&self, command: GitCredentialCommand, input: &str) -> Result<String>;
}

#[derive(Debug, Default)]
pub(crate) struct SystemGitCredentialExecutor;

impl GitCredentialExecutor for SystemGitCredentialExecutor {
    fn run(&self, command: GitCredentialCommand, input: &str) -> Result<String> {
        run_host_git_credential(command, input)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitCredentialHostRequest {
    pub(crate) action: GitCredentialAction,
    pub(crate) input: String,
}

#[derive(Debug)]
pub(crate) struct GitCredentialRuntime {
    mounts: Vec<DockerMountSpec>,
    cleanup_paths: Vec<PathBuf>,
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
    mounts: Vec<DockerMountSpec>,
    container_env: BTreeMap<String, String>,
    token_file: Option<PathBuf>,
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
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SshAgentRuntime {
    mounts: Vec<DockerMountSpec>,
    container_env: BTreeMap<String, String>,
}

impl SshAgentRuntime {
    fn empty() -> Self {
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

#[derive(Debug, Serialize)]
struct GitCredentialHelperRequest<'a> {
    version: u16,
    #[serde(rename = "type")]
    request_type: &'static str,
    action: GitCredentialAction,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct GitCredentialHelperResponse {
    version: u16,
    ok: bool,
    output: Option<String>,
    error: Option<GitCredentialHelperError>,
}

#[derive(Debug, Deserialize)]
struct GitCredentialHelperError {
    message: String,
}

pub(crate) fn git_credential_helper_request_json(
    action: GitCredentialAction,
    input: &str,
) -> Result<String> {
    serde_json::to_string(&GitCredentialHelperRequest {
        version: crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION,
        request_type: REQUEST_TYPE_CREDENTIAL,
        action,
        input,
    })
    .context("Failed to serialize Git credential helper request")
}

pub(crate) fn parse_git_credential_helper_response(bytes: &[u8]) -> Result<String> {
    let response: GitCredentialHelperResponse =
        serde_json::from_slice(bytes).context("Invalid host daemon response JSON")?;

    if response.version != crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION {
        bail!(
            "Unsupported host daemon protocol version: {}",
            response.version
        );
    }

    if response.ok {
        return Ok(response.output.unwrap_or_default());
    }

    let message = response
        .error
        .map(|error| error.message)
        .unwrap_or_else(|| "Host daemon request failed".to_owned());
    Err(anyhow!(message))
}

pub(crate) fn handle_git_credential_request(
    request: GitCredentialHostRequest,
    executor: &dyn GitCredentialExecutor,
) -> Result<String> {
    executor.run(
        GitCredentialCommand::from_action(request.action),
        &request.input,
    )
}

pub(crate) fn prepare_git_credential_runtime(
    config: &ResolvedConfig,
    runtime_dir: &Path,
) -> Result<GitCredentialRuntime> {
    prepare_git_credential_runtime_with_gitconfig(
        config,
        runtime_dir,
        host_gitconfig_path().as_deref(),
    )
}

pub(crate) fn prepare_ssh_agent_runtime(config: &ResolvedConfig) -> Result<SshAgentRuntime> {
    let socket_path = env::var_os("SSH_AUTH_SOCK")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    prepare_ssh_agent_runtime_with_socket(config, socket_path.as_deref())
}

pub(crate) fn prepare_github_cli_runtime(
    config: &ResolvedConfig,
    runtime_dir: &Path,
) -> Result<GithubCliRuntime> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        return Ok(GithubCliRuntime::empty());
    }

    let token = host_github_auth_token()?;
    prepare_github_cli_runtime_with_token(config, runtime_dir, token.as_deref())
}

pub(crate) fn prepare_github_cli_runtime_with_token(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    token: Option<&str>,
) -> Result<GithubCliRuntime> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        return Ok(GithubCliRuntime::empty());
    }

    let Some(token) = token else {
        return Ok(GithubCliRuntime::empty());
    };
    let Some(token) = normalize_github_token(token) else {
        return Ok(GithubCliRuntime::empty());
    };

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create GitHub CLI runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    set_private_runtime_parent(runtime_dir)?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set GitHub CLI runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;

    let token_dir = runtime_dir.join(GITHUB_CLI_TOKEN_DIR_NAME);
    fs::create_dir_all(&token_dir).with_context(|| {
        format!(
            "Failed to create GitHub CLI token directory: {}",
            token_dir.display()
        )
    })?;
    fs::set_permissions(&token_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set GitHub CLI token directory permissions: {}",
            token_dir.display()
        )
    })?;

    let token_file = token_dir.join(GITHUB_CLI_TOKEN_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&token_file)
        .with_context(|| {
            format!(
                "Failed to create GitHub CLI token file: {}",
                token_file.display()
            )
        })?;
    file.write_all(token.as_bytes()).with_context(|| {
        format!(
            "Failed to write GitHub CLI token file: {}",
            token_file.display()
        )
    })?;
    file.sync_all().with_context(|| {
        format!(
            "Failed to sync GitHub CLI token file: {}",
            token_file.display()
        )
    })?;
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Failed to set GitHub CLI token file permissions: {}",
            token_file.display()
        )
    })?;

    Ok(GithubCliRuntime {
        mounts: vec![
            DockerMountSpec {
                source: Some(token_dir.display().to_string()),
                target: GITHUB_CLI_TOKEN_DIR_TARGET.to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
            DockerMountSpec {
                source: None,
                target: GITHUB_CLI_CONFIG_TARGET.to_owned(),
                mount_type: MountType::Tmpfs,
                read_only: false,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
        ],
        container_env: BTreeMap::from([(
            "GH_CONFIG_DIR".to_owned(),
            GITHUB_CLI_CONFIG_TARGET.to_owned(),
        )]),
        token_file: Some(token_file),
    })
}

pub(crate) fn prepare_ssh_agent_runtime_with_socket(
    config: &ResolvedConfig,
    socket_path: Option<&Path>,
) -> Result<SshAgentRuntime> {
    if !config.credentials.git.enabled || config.credentials.git.ssh_agent == SshAgentMode::Off {
        return Ok(SshAgentRuntime::empty());
    }

    let Some(socket_path) = socket_path else {
        return ssh_agent_unavailable(config, "SSH_AUTH_SOCK is not available");
    };

    match inspect_ssh_agent_socket(socket_path)? {
        SshAgentSocketStatus::Available => {}
        SshAgentSocketStatus::Missing => {
            return ssh_agent_unavailable(config, "SSH_AUTH_SOCK is not available");
        }
        SshAgentSocketStatus::NotSocket => {
            return ssh_agent_unavailable(config, "SSH_AUTH_SOCK is not a Unix socket");
        }
    }

    Ok(SshAgentRuntime {
        mounts: vec![DockerMountSpec {
            source: Some(socket_path.display().to_string()),
            target: SSH_AGENT_SOCKET_TARGET.to_owned(),
            mount_type: crate::config::types::MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }],
        container_env: BTreeMap::from([(
            "SSH_AUTH_SOCK".to_owned(),
            SSH_AGENT_SOCKET_TARGET.to_owned(),
        )]),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshAgentSocketStatus {
    Available,
    Missing,
    NotSocket,
}

fn inspect_ssh_agent_socket(socket_path: &Path) -> Result<SshAgentSocketStatus> {
    let metadata = match fs::metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SshAgentSocketStatus::Missing);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect SSH_AUTH_SOCK path: {}",
                    socket_path.display()
                )
            });
        }
    };

    if metadata.file_type().is_socket() {
        Ok(SshAgentSocketStatus::Available)
    } else {
        Ok(SshAgentSocketStatus::NotSocket)
    }
}

fn ssh_agent_unavailable(config: &ResolvedConfig, reason: &str) -> Result<SshAgentRuntime> {
    match config.credentials.git.ssh_agent {
        SshAgentMode::Required => bail!("SSH agent forwarding is required, but {reason}"),
        SshAgentMode::Auto | SshAgentMode::Off => Ok(SshAgentRuntime::empty()),
    }
}

fn prepare_git_credential_runtime_with_gitconfig(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    host_gitconfig: Option<&Path>,
) -> Result<GitCredentialRuntime> {
    let helper_enabled = git_host_helper_enabled(&config.credentials.git);
    let copy_global_config =
        config.credentials.git.enabled && config.credentials.git.copy_global_config;

    if !helper_enabled && !copy_global_config {
        return Ok(GitCredentialRuntime::empty());
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create Git credential runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    set_private_runtime_parent(runtime_dir)?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set Git credential runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;

    let mut cleanup_paths = Vec::new();
    if helper_enabled {
        let helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_NAME);
        fs::write(&helper_path, git_credential_helper_launcher()).with_context(|| {
            format!(
                "Failed to stage Git credential helper: {}",
                helper_path.display()
            )
        })?;
        fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o755)).with_context(
            || {
                format!(
                    "Failed to set Git credential helper permissions: {}",
                    helper_path.display()
                )
            },
        )?;
        cleanup_paths.push(helper_path);

        let linux_x86_64_helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_LINUX_X86_64_NAME);
        fs::write(
            &linux_x86_64_helper_path,
            GIT_CREDENTIAL_HELPER_LINUX_X86_64,
        )
        .with_context(|| {
            format!(
                "Failed to stage Linux x86_64 Git credential helper: {}",
                linux_x86_64_helper_path.display()
            )
        })?;
        fs::set_permissions(&linux_x86_64_helper_path, fs::Permissions::from_mode(0o755))
            .with_context(|| {
                format!(
                    "Failed to set Linux x86_64 Git credential helper permissions: {}",
                    linux_x86_64_helper_path.display()
                )
            })?;
        cleanup_paths.push(linux_x86_64_helper_path);
    }

    if copy_global_config
        && let Some(source) = host_gitconfig
        && source.is_file()
    {
        let target = runtime_dir.join(HOST_GITCONFIG_NAME);
        fs::copy(source, &target).with_context(|| {
            format!(
                "Failed to stage host Git config for credential setup: {}",
                source.display()
            )
        })?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).with_context(|| {
            format!(
                "Failed to set staged host Git config permissions: {}",
                target.display()
            )
        })?;
        cleanup_paths.push(target);
    }

    if cleanup_paths.is_empty() {
        return Ok(GitCredentialRuntime::empty());
    }

    Ok(GitCredentialRuntime {
        mounts: vec![DockerMountSpec {
            source: Some(runtime_dir.display().to_string()),
            target: DECUNE_RUNTIME_TARGET.to_owned(),
            mount_type: crate::config::types::MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }],
        cleanup_paths,
    })
}

pub(crate) async fn setup_git_credentials(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !git_credentials_setup_enabled(&config.credentials.git) {
        return Ok(());
    }

    let script = git_credential_setup_script(&config.credentials.git)?;
    if script.is_empty() {
        return Ok(());
    }

    if git_host_helper_enabled(&config.credentials.git)
        && !git_credential_runtime_accessible(client, container, remote_user).await
    {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}"
        ));
        return Ok(());
    }

    let env = BTreeMap::from([("HOME".to_owned(), remote_user.home.clone())]);
    let setup_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_user.home.clone()),
            env,
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to setup Git credentials in container: {container}"));
    if setup_result.is_err() {
        let message = if git_host_helper_enabled(&config.credentials.git) {
            "Git credential forwarding is unavailable"
        } else {
            "Git credential setup is unavailable"
        };
        ui::warn(&format!("{message} in container: {container}"));
    }

    Ok(())
}

pub(crate) async fn setup_github_cli_credentials(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !github_cli_credentials_enabled(&config.credentials.github) {
        return Ok(());
    }

    if !github_token_file_accessible(client, container).await {
        clear_github_cli_config_dir(client, container).await;
        return Ok(());
    }

    if !github_cli_available(client, container, remote_user).await {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
        if config.credentials.github.install_feature_if_missing {
            ui::warn("GitHub CLI Feature auto-install is not applied yet in this milestone");
        }
        return Ok(());
    }

    if prepare_github_cli_config_dir(client, container, remote_user)
        .await
        .is_err()
    {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
        return Ok(());
    }

    let setup_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                github_cli_setup_script(&config.credentials.github),
            ],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_user.home.clone()),
            env: BTreeMap::from([("HOME".to_owned(), remote_user.home.clone())]),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to setup GitHub CLI credentials in container: {container}"));
    if setup_result.is_err() {
        ui::warn(&format!(
            "GitHub CLI token forwarding is unavailable in container: {container}"
        ));
    }

    Ok(())
}

async fn prepare_github_cli_config_dir(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    let script = format!(
        "set -e\nmkdir -p {config_dir}\nrm -rf {config_dir}/* {config_dir}/.[!.]* {config_dir}/..?* 2>/dev/null || true\ncp {token_file} {config_token_file}\nchown {uid}:{gid} {config_dir} {config_token_file}\nchmod 700 {config_dir}\nchmod 600 {config_token_file}\n",
        config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET),
        token_file = shell_quote(GITHUB_CLI_TOKEN_TARGET),
        config_token_file = shell_quote(GITHUB_CLI_CONFIG_TOKEN_TARGET),
        uid = remote_user.uid,
        gid = remote_user.gid,
    );

    exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| {
        format!("Failed to prepare GitHub CLI config directory in container: {container}")
    })?;

    Ok(())
}

async fn clear_github_cli_config_dir(
    client: &crate::docker::client::DockerClient,
    container: &str,
) {
    let config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET);
    let script = format!(
        "if [ -d {config_dir} ]; then rm -rf {config_dir}/* {config_dir}/.[!.]* {config_dir}/..?* 2>/dev/null || true; fi\n"
    );

    let _ = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await;
}

async fn git_credential_runtime_accessible(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> bool {
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                format!(
                    "test -x {GIT_CREDENTIAL_HELPER_TARGET} && test -w {HOST_DAEMON_SOCKET_TARGET}"
                ),
            ],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_user.home.clone()),
            env: BTreeMap::from([("HOME".to_owned(), remote_user.home.clone())]),
            tty: false,
        },
    )
    .await;

    matches!(output, Ok(output) if output.exit_code == 0)
}

async fn github_token_file_accessible(
    client: &crate::docker::client::DockerClient,
    container: &str,
) -> bool {
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                format!("test -r {}", shell_quote(GITHUB_CLI_TOKEN_TARGET)),
            ],
            user: Some("root".to_owned()),
            working_dir: None,
            env: BTreeMap::new(),
            tty: false,
        },
    )
    .await;

    matches!(output, Ok(output) if output.exit_code == 0)
}

async fn github_cli_available(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> bool {
    let output = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec![
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "command -v gh >/dev/null 2>&1".to_owned(),
            ],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_user.home.clone()),
            env: BTreeMap::from([("HOME".to_owned(), remote_user.home.clone())]),
            tty: false,
        },
    )
    .await;

    matches!(output, Ok(output) if output.exit_code == 0)
}

pub(crate) fn git_credential_setup_script(credentials: &ResolvedGitCredentials) -> Result<String> {
    if !git_credentials_setup_enabled(credentials) {
        return Ok(String::new());
    }

    let mut script = String::from("set -e\n");
    if credentials.copy_global_config {
        script.push_str(
            "if [ -f /run/decune/host-gitconfig ]; then cp /run/decune/host-gitconfig \"$HOME/.gitconfig\"; fi\n",
        );
    }
    if git_host_helper_enabled(credentials) {
        script.push_str(
            "arch=\"$(uname -m 2>/dev/null || true)\"\ncase \"$arch\" in x86_64|amd64) ;; *) echo \"Unsupported Git credential helper container architecture: ${arch:-unknown}\" >&2; exit 1 ;; esac\n",
        );
        script.push_str(
            "git config --global --unset-all credential.helper >/dev/null 2>&1 || true\n",
        );
        script.push_str("git config --global --add credential.helper ");
        script.push_str(&shell_quote(GIT_CREDENTIAL_HELPER_TARGET));
        script.push('\n');
    }

    if credentials.copy_user {
        if let Some(name) = host_git_config_value("user.name")? {
            script.push_str("git config --global user.name ");
            script.push_str(&shell_quote(&name));
            script.push('\n');
        }
        if let Some(email) = host_git_config_value("user.email")? {
            script.push_str("git config --global user.email ");
            script.push_str(&shell_quote(&email));
            script.push('\n');
        }
    }

    Ok(script)
}

fn github_cli_setup_script(credentials: &ResolvedGithubCredentials) -> String {
    if !github_cli_credentials_enabled(credentials) {
        return String::new();
    }

    format!(
        "set -e\ntoken_file={config_token_file}\ncleanup() {{ rm -f \"$token_file\"; }}\ntrap cleanup EXIT\nGH_CONFIG_DIR={config_dir} gh auth login --with-token < \"$token_file\"\nGH_CONFIG_DIR={config_dir} gh auth setup-git\n",
        config_dir = shell_quote(GITHUB_CLI_CONFIG_TARGET),
        config_token_file = shell_quote(GITHUB_CLI_CONFIG_TOKEN_TARGET),
    )
}

pub(crate) fn invoked_as_git_credential_helper() -> bool {
    env::args_os()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        == Some(GIT_CREDENTIAL_HELPER_NAME)
}

pub(crate) fn run_git_credential_helper() -> Result<()> {
    let action = git_credential_action_from_args()?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("Failed to read Git credential helper stdin")?;
    let request = git_credential_helper_request_json(action, &input)?;

    let socket_path = env::var("DECUNE_HOST_DAEMON_SOCKET")
        .unwrap_or_else(|_| HOST_DAEMON_SOCKET_TARGET.to_owned());
    let mut stream = UnixStream::connect(&socket_path).with_context(|| {
        format!("Failed to connect to decune host daemon socket: {socket_path}")
    })?;
    stream
        .write_all(request.as_bytes())
        .context("Failed to write Git credential request to host daemon")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("Failed to close Git credential request stream")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("Failed to read Git credential response from host daemon")?;
    let output = parse_git_credential_helper_response(&response)?;
    std::io::stdout()
        .write_all(output.as_bytes())
        .context("Failed to write Git credential helper stdout")?;

    Ok(())
}

fn git_host_helper_enabled(credentials: &ResolvedGitCredentials) -> bool {
    credentials.enabled && credentials.https == GitHttpsMode::HostHelper
}

fn git_credentials_setup_enabled(credentials: &ResolvedGitCredentials) -> bool {
    credentials.enabled
        && (credentials.https == GitHttpsMode::HostHelper
            || credentials.copy_user
            || credentials.copy_global_config)
}

fn github_cli_credentials_enabled(credentials: &ResolvedGithubCredentials) -> bool {
    credentials.enabled && credentials.mode == GithubCredentialsMode::GhTokenFile
}

fn host_github_auth_token() -> Result<Option<String>> {
    host_github_auth_token_from(Path::new("gh"))
}

fn host_github_auth_token_from(command: &Path) -> Result<Option<String>> {
    let output = match Command::new(command)
        .args(["auth", "token"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            ui::warn(&format!(
                "GitHub CLI token forwarding is unavailable: failed to run host gh auth token: {error}"
            ));
            return Ok(None);
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let token = match String::from_utf8(output.stdout) {
        Ok(token) => token,
        Err(_) => {
            ui::warn(
                "GitHub CLI token forwarding is unavailable: host gh auth token returned non-UTF-8 output",
            );
            return Ok(None);
        }
    };
    Ok(normalize_github_token(&token))
}

fn normalize_github_token(token: &str) -> Option<String> {
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        None
    } else {
        Some(format!("{token}\n"))
    }
}

fn run_host_git_credential(command: GitCredentialCommand, input: &str) -> Result<String> {
    let mut child = Command::new("git")
        .arg("credential")
        .arg(command.as_git_arg())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn host git credential {}",
                command.as_git_arg()
            )
        })?;

    child
        .stdin
        .as_mut()
        .context("Failed to open host git credential stdin")?
        .write_all(input.as_bytes())
        .with_context(|| {
            format!(
                "Failed to write host git credential {} input",
                command.as_git_arg()
            )
        })?;

    let output = child.wait_with_output().with_context(|| {
        format!(
            "Failed to wait for host git credential {}",
            command.as_git_arg()
        )
    })?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        bail!(
            "Host git credential {} failed with exit code {}",
            command.as_git_arg(),
            code
        );
    }

    String::from_utf8(output.stdout).with_context(|| {
        format!(
            "Host git credential {} returned non-UTF-8 output",
            command.as_git_arg()
        )
    })
}

fn host_git_config_value(key: &str) -> Result<Option<String>> {
    host_git_config_value_from(Path::new("git"), key)
}

fn host_git_config_value_from(command: &Path, key: &str) -> Result<Option<String>> {
    let output = match Command::new(command)
        .args(["config", "--global", "--get", key])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            ui::warn(&format!(
                "Host Git config value is unavailable for {key}: {error}"
            ));
            return Ok(None);
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let value = match String::from_utf8(output.stdout) {
        Ok(value) => value,
        Err(_) => {
            ui::warn(&format!("Host Git config value is not UTF-8: {key}"));
            return Ok(None);
        }
    };
    let value = value.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn host_gitconfig_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".gitconfig"))
}

fn set_private_runtime_parent(runtime_dir: &Path) -> Result<()> {
    let Some(parent) = runtime_dir
        .parent()
        .filter(|path| is_decune_runtime_parent(path))
    else {
        return Ok(());
    };

    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set decune runtime parent directory permissions: {}",
            parent.display()
        )
    })
}

fn is_decune_runtime_parent(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name == "decune" || name.starts_with("decune-"))
}

fn git_credential_helper_launcher() -> &'static [u8] {
    b"#!/bin/sh
set -eu
arch=\"$(uname -m 2>/dev/null || true)\"
case \"$arch\" in
  x86_64|amd64)
    exec /run/decune/git-credential-decune-linux-x86_64 \"$@\"
    ;;
  *)
    echo \"Unsupported Git credential helper container architecture: ${arch:-unknown}\" >&2
    exit 1
    ;;
esac
"
}

fn git_credential_action_from_args() -> Result<GitCredentialAction> {
    let mut args = env::args();
    let _program = args.next();
    match args.next().as_deref() {
        Some("get") => Ok(GitCredentialAction::Get),
        Some("store") => Ok(GitCredentialAction::Store),
        Some("erase") => Ok(GitCredentialAction::Erase),
        Some(action) => bail!("Unsupported Git credential helper action: {action}"),
        None => bail!("Git credential helper action is required"),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{
        GIT_CREDENTIAL_HELPER_NAME, GITHUB_CLI_CONFIG_TARGET, GITHUB_CLI_TOKEN_DIR_TARGET,
        GitCredentialAction, GitCredentialCommand, SSH_AGENT_SOCKET_TARGET,
        git_credential_helper_request_json, git_credential_setup_script, github_cli_setup_script,
        host_git_config_value_from, host_github_auth_token_from,
        parse_git_credential_helper_response, prepare_git_credential_runtime,
        prepare_git_credential_runtime_with_gitconfig, prepare_github_cli_runtime_with_token,
        prepare_ssh_agent_runtime_with_socket,
    };
    use crate::config::{
        resolved::ResolvedConfig,
        types::{GitHttpsMode, GithubCredentialsMode, SshAgentMode},
    };

    #[test]
    fn helper_request_json_preserves_git_protocol_input() {
        let json = git_credential_helper_request_json(
            GitCredentialAction::Get,
            "protocol=https\nhost=github.com\n\n",
        )
        .unwrap();

        assert_eq!(
            json,
            r#"{"version":1,"type":"credential","action":"get","input":"protocol=https\nhost=github.com\n\n"}"#
        );
    }

    #[test]
    fn helper_response_returns_credential_output_for_get() {
        let output = parse_git_credential_helper_response(
            br#"{"version":1,"ok":true,"output":"username=octo\npassword=SECRET\n"}"#,
        )
        .unwrap();

        assert_eq!(output, "username=octo\npassword=SECRET\n");
    }

    #[test]
    fn helper_response_error_does_not_echo_credential_output() {
        let error = parse_git_credential_helper_response(
            br#"{"version":1,"ok":false,"error":{"code":"credential_failed","message":"Host git credential fill failed"}}"#,
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("Host git credential fill failed"));
        assert!(!message.contains("SECRET"));
    }

    #[test]
    fn credential_actions_map_to_allowed_git_commands() {
        assert_eq!(
            GitCredentialCommand::from_action(GitCredentialAction::Get),
            GitCredentialCommand::Fill
        );
        assert_eq!(
            GitCredentialCommand::from_action(GitCredentialAction::Store),
            GitCredentialCommand::Approve
        );
        assert_eq!(
            GitCredentialCommand::from_action(GitCredentialAction::Erase),
            GitCredentialCommand::Reject
        );
    }

    #[test]
    fn runtime_stages_container_helper_in_private_runtime_dir() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let runtime =
            prepare_git_credential_runtime(&ResolvedConfig::default(), &runtime_dir).unwrap();
        let helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_NAME);

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(mode(&runtime_dir), 0o700);
        assert_eq!(mode(&helper_path), 0o755);
        assert_ne!(
            fs::read(&helper_path).unwrap(),
            fs::read(current_exe()).unwrap()
        );
    }

    #[test]
    fn setup_script_rejects_unsupported_container_architectures_before_configuring_helper() {
        let config = ResolvedConfig::default();

        let script = git_credential_setup_script(&config.credentials.git).unwrap();

        let arch_guard = script
            .find("Unsupported Git credential helper container architecture")
            .unwrap();
        let helper_config = script
            .find("git config --global --add credential.helper")
            .unwrap();
        assert!(script.contains("x86_64|amd64)"));
        assert!(arch_guard < helper_config);
    }

    #[test]
    fn host_gitconfig_is_readable_by_remote_user_when_copied() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".gitconfig"), "[user]\n\tname = Octo\n").unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.copy_global_config = true;

        let _runtime = prepare_git_credential_runtime_with_gitconfig(
            &config,
            &runtime_dir,
            Some(&home.join(".gitconfig")),
        )
        .unwrap();

        assert_eq!(mode(&runtime_dir.join("host-gitconfig")), 0o644);
    }

    #[test]
    fn host_gitconfig_is_staged_when_https_is_off_and_copy_global_config_is_enabled() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".gitconfig"), "[user]\n\tname = Octo\n").unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        config.credentials.git.copy_user = false;
        config.credentials.git.copy_global_config = true;

        let runtime = prepare_git_credential_runtime_with_gitconfig(
            &config,
            &runtime_dir,
            Some(&home.join(".gitconfig")),
        )
        .unwrap();

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(runtime.mounts()[0].target, "/run/decune");
        assert_eq!(mode(&runtime_dir.join("host-gitconfig")), 0o644);
    }

    #[test]
    fn setup_script_runs_copy_global_config_when_https_is_off() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        config.credentials.git.copy_user = false;
        config.credentials.git.copy_global_config = true;

        let script = git_credential_setup_script(&config.credentials.git).unwrap();

        assert!(script.contains("cp /run/decune/host-gitconfig \"$HOME/.gitconfig\""));
        assert!(!script.contains("credential.helper"));
        assert!(!script.contains("Unsupported Git credential helper container architecture"));
    }

    #[test]
    fn missing_host_git_is_treated_as_absent_user_config() {
        let missing_git = PathBuf::from("/definitely/missing/decune-test-git");

        let value = host_git_config_value_from(&missing_git, "user.name").unwrap();

        assert_eq!(value, None);
    }

    #[test]
    fn unexecutable_host_git_is_treated_as_absent_user_config() {
        let temp = TempDir::new().unwrap();
        let git = temp.path().join("git");
        fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o644)).unwrap();

        let value = host_git_config_value_from(&git, "user.name").unwrap();

        assert_eq!(value, None);
    }

    #[test]
    fn non_utf8_host_git_config_output_is_treated_as_absent_user_config() {
        let temp = TempDir::new().unwrap();
        let git = temp.path().join("git");
        fs::write(&git, "#!/bin/sh\nprintf '\\377\\376'\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();

        let value = host_git_config_value_from(&git, "user.name").unwrap();

        assert_eq!(value, None);
    }

    #[test]
    fn missing_host_gh_is_treated_as_absent_token() {
        let missing_gh = PathBuf::from("/definitely/missing/decune-test-gh");

        let token = host_github_auth_token_from(&missing_gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn unexecutable_host_gh_is_treated_as_absent_token() {
        let temp = TempDir::new().unwrap();
        let gh = temp.path().join("gh");
        fs::write(&gh, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o644)).unwrap();

        let token = host_github_auth_token_from(&gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn non_utf8_host_gh_token_output_is_treated_as_absent_token() {
        let temp = TempDir::new().unwrap();
        let gh = temp.path().join("gh");
        fs::write(&gh, "#!/bin/sh\nprintf '\\377\\376'\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        let token = host_github_auth_token_from(&gh).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn github_runtime_writes_private_token_file_and_read_only_mount() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        let runtime = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("test-secret\n"),
        )
        .unwrap();

        assert_eq!(mode(&runtime_dir), 0o700);
        assert_eq!(mode(runtime.token_file().unwrap()), 0o600);
        assert_eq!(
            fs::read_to_string(runtime.token_file().unwrap()).unwrap(),
            "test-secret\n"
        );
        assert_eq!(runtime.mounts().len(), 2);
        assert!(
            runtime
                .mounts()
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_TOKEN_DIR_TARGET && mount.read_only)
        );
        assert!(
            runtime
                .mounts()
                .iter()
                .any(|mount| mount.target == GITHUB_CLI_CONFIG_TARGET && !mount.read_only)
        );
    }

    #[test]
    fn github_runtime_removes_token_file_on_drop() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let token_path;
        {
            let runtime = prepare_github_cli_runtime_with_token(
                &ResolvedConfig::default(),
                &runtime_dir,
                Some("test-secret\n"),
            )
            .unwrap();
            token_path = runtime.token_file().unwrap().to_owned();
            assert!(token_path.exists());
        }

        assert!(!token_path.exists());
    }

    #[test]
    fn github_runtime_keeps_stable_token_mount_dir_for_refresh() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let first_source;
        let first_token_path;
        {
            let runtime = prepare_github_cli_runtime_with_token(
                &ResolvedConfig::default(),
                &runtime_dir,
                Some("first-secret\n"),
            )
            .unwrap();
            first_source = runtime
                .mounts()
                .iter()
                .find(|mount| mount.target == GITHUB_CLI_TOKEN_DIR_TARGET)
                .and_then(|mount| mount.source.clone())
                .unwrap();
            first_token_path = runtime.token_file().unwrap().to_owned();
        }

        assert!(!first_token_path.exists());
        assert!(Path::new(&first_source).is_dir());

        let runtime = prepare_github_cli_runtime_with_token(
            &ResolvedConfig::default(),
            &runtime_dir,
            Some("second-secret\n"),
        )
        .unwrap();
        let second_source = runtime
            .mounts()
            .iter()
            .find(|mount| mount.target == GITHUB_CLI_TOKEN_DIR_TARGET)
            .and_then(|mount| mount.source.as_deref())
            .unwrap();

        assert_eq!(second_source, first_source);
        assert_eq!(
            fs::read_to_string(runtime.token_file().unwrap()).unwrap(),
            "second-secret\n"
        );
    }

    #[test]
    fn github_runtime_omits_mount_when_disabled() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.github.enabled = false;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
    }

    #[test]
    fn github_runtime_omits_mount_when_mode_is_off() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.github.mode = GithubCredentialsMode::Off;

        let runtime =
            prepare_github_cli_runtime_with_token(&config, &runtime_dir, Some("test-secret\n"))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.token_file().is_none());
    }

    #[test]
    fn github_setup_script_uses_token_file_without_embedding_token() {
        let script = github_cli_setup_script(&ResolvedConfig::default().credentials.github);

        assert!(script.contains("GH_CONFIG_DIR='/run/decune/gh'"));
        assert!(script.contains("token_file='/run/decune/gh/.decune-token'"));
        assert!(script.contains("gh auth login --with-token < \"$token_file\""));
        assert!(script.contains("gh auth setup-git"));
        assert!(script.contains("rm -f \"$token_file\""));
        assert!(!script.contains("/run/decune/gh-token/token"));
        assert!(!script.contains("test-secret"));
    }

    #[test]
    fn github_setup_script_leaves_config_dir_permissions_to_root_preparation() {
        let script = github_cli_setup_script(&ResolvedConfig::default().credentials.github);

        assert!(!script.contains("chmod 700"));
    }

    #[test]
    fn ssh_agent_auto_adds_mount_and_container_env_when_socket_exists() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let runtime =
            prepare_ssh_agent_runtime_with_socket(&ResolvedConfig::default(), Some(&socket_path))
                .unwrap();

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(runtime.mounts()[0].source.as_deref(), socket_path.to_str());
        assert_eq!(runtime.mounts()[0].target, SSH_AGENT_SOCKET_TARGET);
        assert!(!runtime.mounts()[0].read_only);
        assert_eq!(
            runtime
                .container_env()
                .get("SSH_AUTH_SOCK")
                .map(String::as_str),
            Some(SSH_AGENT_SOCKET_TARGET)
        );
    }

    #[test]
    fn ssh_agent_required_errors_when_socket_is_absent() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.ssh_agent = SshAgentMode::Required;

        let error = prepare_ssh_agent_runtime_with_socket(&config, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("SSH agent forwarding is required")
        );
    }

    #[test]
    fn ssh_agent_required_errors_when_socket_path_is_regular_file() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("agent.sock");
        fs::write(&socket_path, "").unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.ssh_agent = SshAgentMode::Required;

        let error = prepare_ssh_agent_runtime_with_socket(&config, Some(&socket_path)).unwrap_err();

        assert!(error.to_string().contains("not a Unix socket"));
    }

    #[test]
    fn ssh_agent_required_errors_when_socket_path_is_directory() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("agent.sock");
        fs::create_dir(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.ssh_agent = SshAgentMode::Required;

        let error = prepare_ssh_agent_runtime_with_socket(&config, Some(&socket_path)).unwrap_err();

        assert!(error.to_string().contains("not a Unix socket"));
    }

    #[test]
    fn ssh_agent_auto_omits_mount_and_container_env_when_socket_is_absent() {
        let runtime =
            prepare_ssh_agent_runtime_with_socket(&ResolvedConfig::default(), None).unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.container_env().is_empty());
    }

    #[test]
    fn ssh_agent_auto_omits_mount_and_container_env_when_socket_path_is_regular_file() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("agent.sock");
        fs::write(&socket_path, "").unwrap();

        let runtime =
            prepare_ssh_agent_runtime_with_socket(&ResolvedConfig::default(), Some(&socket_path))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.container_env().is_empty());
    }

    #[test]
    fn ssh_agent_off_omits_mount_and_container_env() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut config = ResolvedConfig::default();
        config.credentials.git.ssh_agent = SshAgentMode::Off;

        let runtime = prepare_ssh_agent_runtime_with_socket(&config, Some(&socket_path)).unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.container_env().is_empty());
    }

    #[test]
    fn ssh_agent_socket_path_is_not_canonicalized() {
        let temp = TempDir::new().unwrap();
        let real_socket_path = temp.path().join("real-agent.sock");
        let symlink_socket_path = temp.path().join("linked-agent.sock");
        let _listener = UnixListener::bind(&real_socket_path).unwrap();
        std::os::unix::fs::symlink(&real_socket_path, &symlink_socket_path).unwrap();

        let runtime = prepare_ssh_agent_runtime_with_socket(
            &ResolvedConfig::default(),
            Some(&symlink_socket_path),
        )
        .unwrap();

        assert_eq!(
            runtime.mounts()[0].source.as_deref(),
            symlink_socket_path.to_str()
        );
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn current_exe() -> PathBuf {
        std::env::current_exe().unwrap()
    }
}

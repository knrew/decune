use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use decune_container_protocol::{
    GitCredentialAction, GitCredentialHostRequest, HOST_DAEMON_PROTOCOL_VERSION, HostDaemonResponse,
};

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedGitCredentials},
        types::GitHttpsMode,
    },
    docker::{
        exec::{ExecCommandSpec, exec_capture, exec_capture_output},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
    host::{
        container_tools::{ContainerTool, ContainerToolPlatform, stage_container_tool},
        credentials::runtime::{
            DECUNE_RUNTIME_TARGET, GIT_CREDENTIAL_HELPER_NAME, GIT_CREDENTIAL_HELPER_TARGET,
            GitCredentialRuntime, HOST_DAEMON_SOCKET_TARGET, HOST_GITCONFIG_NAME, shell_quote,
        },
        runtime::prepare_private_runtime_dir,
    },
    ui,
};

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

pub(crate) fn git_credential_helper_request_json(
    action: GitCredentialAction,
    input: &str,
) -> Result<String> {
    serde_json::to_string(&GitCredentialHostRequest::new(action, input))
        .context("Failed to serialize Git credential helper request")
}

pub(crate) fn parse_git_credential_helper_response(bytes: &[u8]) -> Result<String> {
    let response: HostDaemonResponse =
        serde_json::from_slice(bytes).context("Invalid host daemon response JSON")?;

    if response.version != HOST_DAEMON_PROTOCOL_VERSION {
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
    platform: ContainerToolPlatform,
) -> Result<GitCredentialRuntime> {
    prepare_git_credential_runtime_with_gitconfig(
        config,
        runtime_dir,
        platform,
        host_gitconfig_path().as_deref(),
    )
}

pub(super) fn prepare_git_credential_runtime_with_gitconfig(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
    host_gitconfig: Option<&Path>,
) -> Result<GitCredentialRuntime> {
    prepare_git_credential_runtime_with_gitconfig_and_tool_dirs(
        config,
        runtime_dir,
        platform,
        host_gitconfig,
        None,
    )
}

pub(super) fn prepare_git_credential_runtime_with_gitconfig_and_tool_dirs(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
    host_gitconfig: Option<&Path>,
    tool_source_dirs: Option<Vec<PathBuf>>,
) -> Result<GitCredentialRuntime> {
    let helper_enabled = git_host_helper_enabled(&config.credentials.git);
    let copy_global_config =
        config.credentials.git.enabled && config.credentials.git.copy_global_config;

    if !helper_enabled && !copy_global_config {
        return Ok(GitCredentialRuntime::empty());
    }

    prepare_private_runtime_dir(runtime_dir, "Git credential")?;

    let mut cleanup_paths = Vec::new();
    if helper_enabled {
        let helper_path = match tool_source_dirs {
            Some(source_dirs) => crate::host::container_tools::stage_container_tool_from_dirs(
                ContainerTool::GitCredentialHelper,
                platform,
                runtime_dir,
                source_dirs,
            )?,
            None => {
                stage_container_tool(ContainerTool::GitCredentialHelper, platform, runtime_dir)?
            }
        };
        cleanup_paths.push(helper_path);
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
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).with_context(|| {
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

    setup_git_credential_helper(client, container, config, remote_user).await?;
    setup_git_user_config(client, container, config, remote_user).await?;
    Ok(())
}

async fn setup_git_credential_helper(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !git_host_helper_enabled(&config.credentials.git) {
        return Ok(());
    }

    let Some(remote_home) = remote_user.home.as_deref() else {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}: remote user home is unavailable"
        ));
        return Ok(());
    };

    if !git_credential_runtime_accessible(client, container, remote_user).await {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}"
        ));
        return Ok(());
    }

    let script = git_credential_helper_setup_script(&config.credentials.git);
    let env = BTreeMap::from([("HOME".to_owned(), remote_home.to_owned())]);
    let setup_result = exec_capture_output(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env,
            tty: false,
        },
    )
    .await;
    match setup_result {
        Ok(output) if output.exit_code == 0 => {}
        Ok(output) => warn_git_credential_setup_unavailable(container, &output.stderr),
        Err(_) => warn_git_credential_setup_unavailable(container, &[]),
    }

    Ok(())
}

fn warn_git_credential_setup_unavailable(container: &str, stderr: &[u8]) {
    if let Some(detail) = git_credential_setup_warning_detail(stderr) {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}: {detail}"
        ));
    } else {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}"
        ));
    }
}

fn git_credential_setup_warning_detail(stderr: &[u8]) -> Option<String> {
    const MISSING_TOOL_PREFIX: &str = "Missing Git credential helper container tool:";

    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(MISSING_TOOL_PREFIX))
        .map(str::to_owned)
}

async fn setup_git_user_config(
    client: &crate::docker::client::DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
) -> Result<()> {
    if !git_user_config_copy_enabled(&config.credentials.git) {
        return Ok(());
    }

    let script = git_user_config_setup_script(&config.credentials.git)?;
    if script.is_empty() {
        return Ok(());
    }

    let Some(remote_home) = remote_user.home.as_deref() else {
        ui::warn(&format!(
            "Git user config copy is unavailable in container: {container}: remote user home is unavailable"
        ));
        return Ok(());
    };

    let env = BTreeMap::from([("HOME".to_owned(), remote_home.to_owned())]);
    let setup_result = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env,
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to copy Git user config in container: {container}"));
    if setup_result.is_err() {
        ui::warn(&format!(
            "Git user config copy is unavailable in container: {container}"
        ));
    }

    Ok(())
}

fn git_credential_helper_setup_script(credentials: &ResolvedGitCredentials) -> String {
    if !git_host_helper_enabled(credentials) {
        return String::new();
    }
    let mut script = String::from("set -e\n");
    script.push_str(&format!(
        "test -x {} || {{ echo \"Missing Git credential helper container tool: {}\" >&2; exit 1; }}\n",
        shell_quote(GIT_CREDENTIAL_HELPER_TARGET),
        GIT_CREDENTIAL_HELPER_TARGET
    ));
    script.push_str("git config --global --unset-all credential.helper >/dev/null 2>&1 || true\n");
    script.push_str("git config --global --add credential.helper ");
    script.push_str(&shell_quote(GIT_CREDENTIAL_HELPER_TARGET));
    script.push('\n');
    script
}

fn git_user_config_setup_script(credentials: &ResolvedGitCredentials) -> Result<String> {
    if !git_user_config_copy_enabled(credentials) {
        return Ok(String::new());
    }

    let name = host_git_config_value("user.name")?;
    let email = host_git_config_value("user.email")?;
    Ok(git_user_config_setup_script_from_values(
        credentials,
        name.as_deref(),
        email.as_deref(),
    ))
}

fn git_user_config_setup_script_from_values(
    credentials: &ResolvedGitCredentials,
    name: Option<&str>,
    email: Option<&str>,
) -> String {
    if !git_user_config_copy_enabled(credentials) || (name.is_none() && email.is_none()) {
        return String::new();
    }

    let mut script = String::from("set -e\n");
    if let Some(name) = name {
        script.push_str("git config --global user.name ");
        script.push_str(&shell_quote(name));
        script.push('\n');
    }
    if let Some(email) = email {
        script.push_str("git config --global user.email ");
        script.push_str(&shell_quote(email));
        script.push('\n');
    }

    script
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
    credentials.enabled && (credentials.https == GitHttpsMode::HostHelper || credentials.copy_user)
}

fn git_user_config_copy_enabled(credentials: &ResolvedGitCredentials) -> bool {
    credentials.enabled && credentials.copy_user
}

async fn git_credential_runtime_accessible(
    client: &crate::docker::client::DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) -> bool {
    let Some(remote_home) = remote_user.home.as_deref() else {
        return false;
    };

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
            working_dir: Some(remote_home.to_owned()),
            env: BTreeMap::from([("HOME".to_owned(), remote_home.to_owned())]),
            tty: false,
        },
    )
    .await;

    matches!(output, Ok(output) if output.exit_code == 0)
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use decune_container_protocol::GitCredentialAction;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        config::{resolved::ResolvedConfig, types::GitHttpsMode},
        host::container_tools::ContainerToolPlatform,
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
        let source_dir = temp.path().join("tools");
        crate::host::container_tools::write_test_container_tools_bundle(
            &source_dir,
            &[crate::host::container_tools::TestContainerToolEntry {
                tool: ContainerTool::GitCredentialHelper,
                platform: ContainerToolPlatform::LinuxAmd64,
                contents: b"helper",
            }],
        )
        .unwrap();
        let runtime_dir = temp.path().join("runtime");
        let runtime = prepare_git_credential_runtime_with_gitconfig_and_tool_dirs(
            &ResolvedConfig::default(),
            &runtime_dir,
            ContainerToolPlatform::LinuxAmd64,
            None,
            Some(vec![source_dir]),
        )
        .unwrap();
        let helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_NAME);

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(mode(&runtime_dir), 0o700);
        assert_eq!(mode(&helper_path), 0o755);
        assert_eq!(
            fs::read(runtime_dir.join("git-credential-decune")).unwrap(),
            b"helper"
        );
        assert_eq!(fs::read(&helper_path).unwrap(), b"helper");
    }

    #[test]
    fn helper_setup_script_requires_staged_real_binary_before_configuring_helper() {
        let config = ResolvedConfig::default();

        let script = git_credential_helper_setup_script(&config.credentials.git);

        let helper_guard_command = format!("test -x {}", shell_quote(GIT_CREDENTIAL_HELPER_TARGET));
        let helper_guard = script.find(&helper_guard_command).unwrap();
        let helper_config = script
            .find("git config --global --add credential.helper")
            .unwrap();
        assert_eq!(
            script
                .matches("git config --global --add credential.helper")
                .count(),
            1
        );
        assert!(helper_guard < helper_config);
    }

    #[test]
    fn user_config_setup_script_is_independent_from_helper_architecture_guard() {
        let config = ResolvedConfig::default();

        let helper_script = git_credential_helper_setup_script(&config.credentials.git);
        let user_script = git_user_config_setup_script_from_values(
            &config.credentials.git,
            Some("Octo User"),
            Some("octo@example.test"),
        );

        assert!(helper_script.contains("Missing Git credential helper container tool"));
        assert!(user_script.contains("git config --global user.name 'Octo User'"));
        assert!(user_script.contains("git config --global user.email 'octo@example.test'"));
        assert!(!user_script.contains("credential.helper"));
        assert!(!user_script.contains("Missing Git credential helper container tool"));
    }

    #[test]
    fn setup_warning_detail_preserves_missing_container_tool() {
        let detail = git_credential_setup_warning_detail(
            b"Missing Git credential helper container tool: /run/decune/git-credential-decune\n",
        );

        assert_eq!(
            detail.as_deref(),
            Some("Missing Git credential helper container tool: /run/decune/git-credential-decune")
        );
    }

    #[test]
    fn host_gitconfig_is_staged_privately_when_copied() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".gitconfig"), "[user]\n\tname = Octo\n").unwrap();
        let runtime_dir = temp.path().join("runtime");
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        config.credentials.git.copy_user = false;
        config.credentials.git.copy_global_config = true;

        let _runtime = prepare_git_credential_runtime_with_gitconfig(
            &config,
            &runtime_dir,
            ContainerToolPlatform::LinuxAmd64,
            Some(&home.join(".gitconfig")),
        )
        .unwrap();

        assert_eq!(mode(&runtime_dir.join("host-gitconfig")), 0o600);
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
            ContainerToolPlatform::LinuxAmd64,
            Some(&home.join(".gitconfig")),
        )
        .unwrap();

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(runtime.mounts()[0].target, "/run/decune");
        assert_eq!(mode(&runtime_dir.join("host-gitconfig")), 0o600);
    }

    #[test]
    fn setup_script_omits_copy_global_config_when_https_is_off() {
        let mut config = ResolvedConfig::default();
        config.credentials.git.https = GitHttpsMode::Off;
        config.credentials.git.copy_user = false;
        config.credentials.git.copy_global_config = true;

        let script = git_credential_helper_setup_script(&config.credentials.git);

        assert!(script.is_empty());
        assert!(!script.contains("credential.helper"));
        assert!(!script.contains("Missing Git credential helper container tool"));
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

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

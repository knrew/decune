use std::{
    env, fs, io,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::{
        resolved::{ResolvedConfig, ResolvedGitCredentials},
        types::GitHttpsMode,
    },
    docker::{mounts::DockerMountSpec, user::ResolvedRemoteUser},
    ui,
};

pub(crate) const DECUNE_RUNTIME_TARGET: &str = "/run/decune";
const GIT_CREDENTIAL_HELPER_NAME: &str = "git-credential-decune";
const GIT_CREDENTIAL_HELPER_LINUX_X86_64_NAME: &str = "git-credential-decune-linux-x86_64";
const HOST_GITCONFIG_NAME: &str = "host-gitconfig";
const HOST_DAEMON_SOCKET_TARGET: &str = "/run/decune/host-daemon.sock";
const GIT_CREDENTIAL_HELPER_TARGET: &str = "/run/decune/git-credential-decune";
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

fn prepare_git_credential_runtime_with_gitconfig(
    config: &ResolvedConfig,
    runtime_dir: &Path,
    host_gitconfig: Option<&Path>,
) -> Result<GitCredentialRuntime> {
    if !git_host_helper_enabled(&config.credentials.git) {
        return Ok(GitCredentialRuntime::empty());
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create Git credential runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    set_private_runtime_parent(runtime_dir)?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "Failed to set Git credential runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;

    let helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_NAME);
    fs::write(&helper_path, git_credential_helper_launcher()).with_context(|| {
        format!(
            "Failed to stage Git credential helper: {}",
            helper_path.display()
        )
    })?;
    fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "Failed to set Git credential helper permissions: {}",
            helper_path.display()
        )
    })?;

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

    let mut cleanup_paths = vec![helper_path, linux_x86_64_helper_path];
    if config.credentials.git.copy_global_config
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
    if !git_host_helper_enabled(&config.credentials.git) {
        return Ok(());
    }

    let script = git_credential_setup_script(&config.credentials.git)?;
    if script.is_empty() {
        return Ok(());
    }

    let setup_result = crate::docker::exec::exec_capture(
        client,
        container,
        &crate::docker::exec::ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_user.home.clone()),
            env: Default::default(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to setup Git credentials in container: {container}"));
    if setup_result.is_err() {
        ui::warn(&format!(
            "Git credential forwarding is unavailable in container: {container}"
        ));
    }

    Ok(())
}

pub(crate) fn git_credential_setup_script(credentials: &ResolvedGitCredentials) -> Result<String> {
    if !git_host_helper_enabled(credentials) {
        return Ok(String::new());
    }

    let mut script = String::from("set -e\n");
    if credentials.copy_global_config {
        script.push_str(
            "if [ -f /run/decune/host-gitconfig ]; then cp /run/decune/host-gitconfig \"$HOME/.gitconfig\"; fi\n",
        );
    }
    script.push_str("git config --global --unset-all credential.helper >/dev/null 2>&1 || true\n");
    script.push_str("git config --global --add credential.helper ");
    script.push_str(&shell_quote(GIT_CREDENTIAL_HELPER_TARGET));
    script.push('\n');

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
            return Err(error)
                .with_context(|| format!("Failed to read host Git config value: {key}"));
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8(output.stdout)
        .with_context(|| format!("Host Git config value is not UTF-8: {key}"))?;
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
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{
        GIT_CREDENTIAL_HELPER_NAME, GitCredentialAction, GitCredentialCommand,
        git_credential_helper_request_json, host_git_config_value_from,
        parse_git_credential_helper_response, prepare_git_credential_runtime,
        prepare_git_credential_runtime_with_gitconfig,
    };
    use crate::config::resolved::ResolvedConfig;

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
    fn runtime_stages_container_helper_with_remote_user_permissions() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        let runtime =
            prepare_git_credential_runtime(&ResolvedConfig::default(), &runtime_dir).unwrap();
        let helper_path = runtime_dir.join(GIT_CREDENTIAL_HELPER_NAME);

        assert_eq!(runtime.mounts().len(), 1);
        assert_eq!(mode(&runtime_dir), 0o755);
        assert_eq!(mode(&helper_path), 0o755);
        assert_ne!(
            fs::read(&helper_path).unwrap(),
            fs::read(current_exe()).unwrap()
        );
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
    fn missing_host_git_is_treated_as_absent_user_config() {
        let missing_git = PathBuf::from("/definitely/missing/decune-test-git");

        let value = host_git_config_value_from(&missing_git, "user.name").unwrap();

        assert_eq!(value, None);
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn current_exe() -> PathBuf {
        std::env::current_exe().unwrap()
    }
}

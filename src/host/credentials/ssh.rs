use std::{
    collections::BTreeMap,
    env, fs, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        resolved::ResolvedConfig,
        types::{MountType, SshAgentMode},
    },
    docker::mounts::DockerMountSpec,
    host::credentials::runtime::{SSH_AGENT_SOCKET_TARGET, SshAgentRuntime},
    ui,
};

pub(crate) fn prepare_ssh_agent_runtime(config: &ResolvedConfig) -> Result<SshAgentRuntime> {
    let socket_path = env::var_os("SSH_AUTH_SOCK")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    prepare_ssh_agent_runtime_with_socket(config, socket_path.as_deref())
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

    let socket_status = match inspect_ssh_agent_socket(socket_path) {
        Ok(status) => status,
        Err(error) => return ssh_agent_inspect_failed(config, error),
    };
    match socket_status {
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
            mount_type: MountType::Bind,
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

fn ssh_agent_inspect_failed(
    config: &ResolvedConfig,
    error: anyhow::Error,
) -> Result<SshAgentRuntime> {
    match config.credentials.git.ssh_agent {
        SshAgentMode::Required => Err(error)
            .context("SSH agent forwarding is required, but failed to inspect SSH_AUTH_SOCK"),
        SshAgentMode::Auto => {
            ui::warn(&format!("SSH agent forwarding is unavailable: {error:#}"));
            Ok(SshAgentRuntime::empty())
        }
        SshAgentMode::Off => Ok(SshAgentRuntime::empty()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::{ffi::OsStringExt, net::UnixListener},
        path::PathBuf,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        config::{resolved::ResolvedConfig, types::SshAgentMode},
        host::credentials::runtime::SSH_AGENT_SOCKET_TARGET,
    };

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
    fn ssh_agent_auto_omits_mount_and_container_env_when_socket_inspection_fails() {
        let socket_path = invalid_path_with_nul();

        let runtime =
            prepare_ssh_agent_runtime_with_socket(&ResolvedConfig::default(), Some(&socket_path))
                .unwrap();

        assert!(runtime.mounts().is_empty());
        assert!(runtime.container_env().is_empty());
    }

    #[test]
    fn ssh_agent_required_errors_when_socket_inspection_fails() {
        let socket_path = invalid_path_with_nul();
        let mut config = ResolvedConfig::default();
        config.credentials.git.ssh_agent = SshAgentMode::Required;

        let error = prepare_ssh_agent_runtime_with_socket(&config, Some(&socket_path)).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("SSH agent forwarding is required"));
        assert!(message.contains("Failed to inspect SSH_AUTH_SOCK"));
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

    fn invalid_path_with_nul() -> PathBuf {
        PathBuf::from(OsString::from_vec(b"invalid\0socket".to_vec()))
    }
}

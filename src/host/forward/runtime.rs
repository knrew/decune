use std::{
    collections::BTreeSet,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::types::MountType,
    docker::ports::ResolvedForwardPort,
    hex::hex_lower,
    host::{
        container_tools::{ContainerTool, ContainerToolPlatform, stage_container_tool},
        credentials::DECUNE_RUNTIME_TARGET,
        runtime::prepare_private_runtime_dir,
    },
};

use super::{
    FORWARD_AGENT_ALLOWED_PORTS_ENV, FORWARD_AGENT_DIAGNOSTIC_NAME, FORWARD_AGENT_SECRET_ENV,
    FORWARD_AGENT_SOCKET_ENV, FORWARD_AGENT_SOCKET_NAME, FORWARD_AGENT_STATUS_NAME,
    FORWARD_AGENT_TARGET, FORWARD_AGENT_USER, forward_agent_socket_name,
    service_forward_runtime_dir,
};

#[derive(Debug)]
pub(crate) struct ForwardRuntime {
    mounts: Vec<crate::docker::mounts::DockerMountSpec>,
    cleanup_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct ServiceForwardRuntime {
    service: String,
    mount: crate::docker::mounts::DockerMountSpec,
    cleanup_paths: Vec<PathBuf>,
}

impl ForwardRuntime {
    pub(crate) fn mounts(&self) -> &[crate::docker::mounts::DockerMountSpec] {
        &self.mounts
    }
}

impl ServiceForwardRuntime {
    pub(crate) fn service(&self) -> &str {
        &self.service
    }

    pub(crate) const fn mount(&self) -> &crate::docker::mounts::DockerMountSpec {
        &self.mount
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        service: impl Into<String>,
        mount: crate::docker::mounts::DockerMountSpec,
    ) -> Self {
        Self {
            service: service.into(),
            mount,
            cleanup_paths: Vec::new(),
        }
    }
}

impl Drop for ForwardRuntime {
    fn drop(&mut self) {
        for path in &self.cleanup_paths {
            _ = fs::remove_file(path);
        }
    }
}

impl Drop for ServiceForwardRuntime {
    fn drop(&mut self) {
        for path in &self.cleanup_paths {
            _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn prepare_forward_runtime(
    _forward_ports: &[ResolvedForwardPort],
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
) -> Result<ForwardRuntime> {
    prepare_forward_runtime_with_tool_dirs(runtime_dir, platform, None)
}

fn prepare_forward_runtime_with_tool_dirs(
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
    tool_source_dirs: Option<Vec<PathBuf>>,
) -> Result<ForwardRuntime> {
    prepare_private_runtime_dir(runtime_dir, "port forwarding")?;
    remove_stale_agent_start_file(&runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME))?;
    remove_stale_agent_start_file(&runtime_dir.join(FORWARD_AGENT_STATUS_NAME))?;
    let agent_path = match tool_source_dirs {
        Some(source_dirs) => crate::host::container_tools::stage_container_tool_from_dirs(
            ContainerTool::ForwardAgent,
            platform,
            runtime_dir,
            &source_dirs,
        )?,
        None => stage_container_tool(ContainerTool::ForwardAgent, platform, runtime_dir)?,
    };

    Ok(ForwardRuntime {
        mounts: vec![crate::docker::mounts::DockerMountSpec {
            source: Some(runtime_dir.display().to_string()),
            target: DECUNE_RUNTIME_TARGET.to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }],
        cleanup_paths: std::iter::once(agent_path)
            .chain([
                runtime_dir.join(FORWARD_AGENT_SOCKET_NAME),
                runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME),
                runtime_dir.join(FORWARD_AGENT_STATUS_NAME),
            ])
            .collect(),
    })
}

pub(crate) fn prepare_service_forward_runtimes(
    forward_ports: &[ResolvedForwardPort],
    primary_service: Option<&str>,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
) -> Result<Vec<ServiceForwardRuntime>> {
    let Some(primary_service) = primary_service else {
        return Ok(Vec::new());
    };
    let services = forward_ports
        .iter()
        .filter_map(|port| port.service.as_deref())
        .filter(|service| *service != primary_service)
        .collect::<BTreeSet<_>>();
    services
        .into_iter()
        .map(|service| prepare_service_forward_runtime(service, runtime_dir, platform))
        .collect()
}

fn prepare_service_forward_runtime(
    service: &str,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
) -> Result<ServiceForwardRuntime> {
    let runtime_dir = service_forward_runtime_dir(runtime_dir, service);
    prepare_private_runtime_dir(&runtime_dir, "service port forwarding")?;
    remove_stale_agent_start_file(&runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME))?;
    remove_stale_agent_start_file(&runtime_dir.join(FORWARD_AGENT_STATUS_NAME))?;
    let agent_path = stage_container_tool(ContainerTool::ForwardAgent, platform, &runtime_dir)?;
    let socket_path = runtime_dir.join(forward_agent_socket_name(Some(service)));
    remove_stale_agent_start_file(&socket_path)?;

    Ok(ServiceForwardRuntime {
        service: service.to_owned(),
        mount: crate::docker::mounts::DockerMountSpec {
            source: Some(runtime_dir.display().to_string()),
            target: DECUNE_RUNTIME_TARGET.to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        },
        cleanup_paths: vec![
            agent_path,
            socket_path,
            runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME),
            runtime_dir.join(FORWARD_AGENT_STATUS_NAME),
        ],
    })
}

fn remove_stale_agent_start_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove stale port forwarding agent state file: {}",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
pub(crate) fn forward_agent_command(
    forward_ports: &[ResolvedForwardPort],
    secret: &str,
) -> crate::docker::exec::ExecCommandSpec {
    forward_agent_command_at(forward_ports, secret, super::FORWARD_AGENT_SOCKET_TARGET)
}

pub(crate) fn forward_agent_command_at(
    forward_ports: &[ResolvedForwardPort],
    secret: &str,
    socket_target: &str,
) -> crate::docker::exec::ExecCommandSpec {
    crate::docker::exec::ExecCommandSpec {
        command: vec![FORWARD_AGENT_TARGET.to_owned()],
        user: Some(FORWARD_AGENT_USER.to_owned()),
        working_dir: None,
        env: std::collections::BTreeMap::from([
            (
                FORWARD_AGENT_ALLOWED_PORTS_ENV.to_owned(),
                allowed_ports_env(forward_ports),
            ),
            (FORWARD_AGENT_SECRET_ENV.to_owned(), secret.to_owned()),
            (
                FORWARD_AGENT_SOCKET_ENV.to_owned(),
                socket_target.to_owned(),
            ),
        ]),
        redactions: vec![secret.to_owned()],
        tty: false,
    }
}

pub(crate) fn new_forward_agent_secret() -> Result<String> {
    random_hex(32, "port forwarding secret")
}

pub(crate) fn new_forward_agent_socket_id() -> Result<String> {
    random_hex(8, "port forwarding socket id")
}

fn random_hex(bytes_len: usize, context: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    let bytes = &mut bytes[..bytes_len];
    fs::File::open("/dev/urandom")
        .with_context(|| format!("Failed to open /dev/urandom for {context}"))?
        .read_exact(bytes)
        .with_context(|| format!("Failed to read {context}"))?;
    Ok(hex_lower(bytes))
}

fn allowed_ports_env(forward_ports: &[ResolvedForwardPort]) -> String {
    forward_ports
        .iter()
        .map(|port| port.container)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|port| port.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use tempfile::TempDir;

    use super::*;
    use crate::host::{
        container_tools::{ContainerToolPlatform, TestContainerToolEntry},
        credentials::DECUNE_RUNTIME_TARGET,
    };

    #[test]
    fn runtime_stages_container_agent_even_without_forward_ports() {
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("tools");
        crate::host::container_tools::write_test_container_tools_bundle(
            &source_dir,
            &[TestContainerToolEntry {
                tool: ContainerTool::ForwardAgent,
                platform: ContainerToolPlatform::LinuxAmd64,
                contents: b"agent",
            }],
        )
        .unwrap();
        let runtime_dir = temp.path().join("runtime");

        let runtime = prepare_forward_runtime_with_tool_dirs(
            &runtime_dir,
            ContainerToolPlatform::LinuxAmd64,
            Some(vec![source_dir]),
        )
        .unwrap();

        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert_eq!(
            fs::read_dir(&runtime_dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().unwrap().is_file())
                .count(),
            1
        );
        assert_eq!(
            fs::read(runtime_dir.join("decune-forward-agent")).unwrap(),
            b"agent"
        );
        assert_eq!(mode(&runtime_dir), 0o700);
        assert!(runtime.mounts().iter().any(|mount| {
            mount.target == DECUNE_RUNTIME_TARGET
                && mount.source.as_deref() == Some(runtime_dir.to_str().unwrap())
                && !mount.read_only
        }));
    }

    #[test]
    fn forward_agent_command_runs_as_root_for_runtime_mount_access() {
        let spec = forward_agent_command(
            &[crate::host::forward::tests::forward_port(54321, 4321)],
            "test-secret",
        );

        assert_eq!(spec.user.as_deref(), Some("0"));
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

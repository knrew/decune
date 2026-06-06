use std::{
    collections::BTreeSet,
    fs,
    io::Read as _,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::types::MountType,
    docker::ports::ResolvedForwardPort,
    host::{
        container_tools::{
            ContainerTool, stage_container_tool_variants, stage_container_tool_variants_from_dirs,
        },
        credentials::DECUNE_RUNTIME_TARGET,
        runtime::prepare_private_runtime_dir,
    },
};

use super::{
    FORWARD_AGENT_ALLOWED_PORTS_ENV, FORWARD_AGENT_DIAGNOSTIC_NAME, FORWARD_AGENT_NAME,
    FORWARD_AGENT_SECRET_ENV, FORWARD_AGENT_SOCKET_NAME, FORWARD_AGENT_TARGET, FORWARD_AGENT_USER,
};

#[derive(Debug)]
pub(crate) struct ForwardRuntime {
    mounts: Vec<crate::docker::mounts::DockerMountSpec>,
    cleanup_paths: Vec<PathBuf>,
}

impl ForwardRuntime {
    pub(crate) fn mounts(&self) -> &[crate::docker::mounts::DockerMountSpec] {
        &self.mounts
    }
}

impl Drop for ForwardRuntime {
    fn drop(&mut self) {
        for path in &self.cleanup_paths {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn prepare_forward_runtime(
    _forward_ports: &[ResolvedForwardPort],
    runtime_dir: &Path,
) -> Result<ForwardRuntime> {
    prepare_forward_runtime_with_tool_dirs(runtime_dir, None)
}

fn prepare_forward_runtime_with_tool_dirs(
    runtime_dir: &Path,
    tool_source_dirs: Option<Vec<PathBuf>>,
) -> Result<ForwardRuntime> {
    prepare_private_runtime_dir(runtime_dir, "port forwarding")?;
    let agent_path = runtime_dir.join(FORWARD_AGENT_NAME);
    fs::write(&agent_path, forward_agent_launcher()).with_context(|| {
        format!(
            "Failed to stage port forwarding agent: {}",
            agent_path.display()
        )
    })?;
    fs::set_permissions(&agent_path, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "Failed to set port forwarding agent permissions: {}",
            agent_path.display()
        )
    })?;
    let staged_agents = match tool_source_dirs {
        Some(source_dirs) => stage_container_tool_variants_from_dirs(
            ContainerTool::ForwardAgent,
            runtime_dir,
            source_dirs,
        )?,
        None => stage_container_tool_variants(ContainerTool::ForwardAgent, runtime_dir)?,
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
            .chain(staged_agents)
            .chain([
                runtime_dir.join(FORWARD_AGENT_SOCKET_NAME),
                runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME),
            ])
            .collect(),
    })
}

pub(crate) fn forward_agent_command(
    forward_ports: &[ResolvedForwardPort],
    secret: &str,
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
        ]),
        tty: false,
    }
}

pub(crate) fn new_forward_agent_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .context("Failed to open /dev/urandom for port forwarding secret")?
        .read_exact(&mut bytes)
        .context("Failed to read port forwarding secret")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn forward_agent_launcher() -> &'static [u8] {
    b"#!/bin/sh
set -eu
diag=\"/run/decune/forward-agent.err\"
: > \"$diag\" 2>/dev/null || true
arch=\"$(uname -m 2>/dev/null || true)\"
case \"$arch\" in
  x86_64|amd64)
    agent=/run/decune/decune-forward-agent-linux-amd64
    ;;
  aarch64|arm64)
    agent=/run/decune/decune-forward-agent-linux-arm64
    ;;
  *)
    message=\"Unsupported port forwarding agent container architecture: ${arch:-unknown}\"
    echo \"$message\" >&2
    echo \"$message\" >> \"$diag\" 2>/dev/null || true
    exit 1
    ;;
esac
if [ ! -x \"$agent\" ]; then
    message=\"Missing port forwarding agent container tool: $agent\"
    echo \"$message\" >&2
    echo \"$message\" >> \"$diag\" 2>/dev/null || true
    exit 1
fi
exec \"$agent\" \"$@\" 2>>\"$diag\"
"
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
    use std::{env, fs, os::unix::fs::PermissionsExt, path::Path};

    use tempfile::TempDir;

    use super::*;
    use crate::host::credentials::DECUNE_RUNTIME_TARGET;

    #[test]
    fn runtime_stages_container_agent_even_without_forward_ports() {
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("tools");
        write_container_tool(&source_dir, "linux-amd64", FORWARD_AGENT_NAME, b"agent");
        let runtime_dir = temp.path().join("runtime");

        let runtime =
            prepare_forward_runtime_with_tool_dirs(&runtime_dir, Some(vec![source_dir])).unwrap();

        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(
            runtime_dir
                .join("decune-forward-agent-linux-amd64")
                .is_file()
        );
        assert_eq!(
            fs::read(runtime_dir.join("decune-forward-agent-linux-amd64")).unwrap(),
            b"agent"
        );
        assert_ne!(
            fs::read(runtime_dir.join("decune-forward-agent")).unwrap(),
            fs::read(current_exe().unwrap()).unwrap()
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

    fn current_exe() -> Result<PathBuf> {
        env::current_exe().context("Failed to locate current decune executable")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write_container_tool(source_dir: &Path, platform: &str, name: &str, contents: &[u8]) {
        let platform_dir = source_dir.join(platform);
        fs::create_dir_all(&platform_dir).unwrap();
        fs::write(platform_dir.join(name), contents).unwrap();
    }
}

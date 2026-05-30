use std::{
    env, fs, io,
    net::IpAddr,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    task::JoinHandle,
    time::sleep,
};

use crate::{
    config::types::{MountType, PortProtocol},
    docker::ports::ResolvedForwardPort,
    host::credentials::DECUNE_RUNTIME_TARGET,
};

const FORWARD_AGENT_NAME: &str = "decune-forward-agent";
const FORWARD_AGENT_SOCKET_NAME: &str = "forward-agent.sock";
const FORWARD_AGENT_SOCKET_TARGET: &str = "/run/decune/forward-agent.sock";
const FORWARD_AGENT_TARGET: &str = "/run/decune/decune-forward-agent";
const FORWARD_AGENT_START_RETRIES: usize = 100;
const FORWARD_AGENT_START_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(crate) struct ForwardRuntime {
    mounts: Vec<crate::docker::mounts::DockerMountSpec>,
    cleanup_paths: Vec<PathBuf>,
}

impl ForwardRuntime {
    pub(crate) fn empty() -> Self {
        Self {
            mounts: Vec::new(),
            cleanup_paths: Vec::new(),
        }
    }

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

#[derive(Debug)]
pub(crate) struct ForwardSession {
    agent_socket_path: PathBuf,
    listeners: Vec<ForwardListener>,
}

#[derive(Debug)]
struct ForwardListener {
    task: JoinHandle<()>,
    #[cfg(test)]
    local_addr: std::net::SocketAddr,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardAgentRequest {
    port: Option<u16>,
    shutdown: Option<bool>,
}

pub(crate) fn invoked_as_forward_agent() -> bool {
    env::args_os()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        == Some(FORWARD_AGENT_NAME)
}

pub(crate) fn prepare_forward_runtime(
    forward_ports: &[ResolvedForwardPort],
    runtime_dir: &Path,
) -> Result<ForwardRuntime> {
    if forward_ports.is_empty() {
        return Ok(ForwardRuntime::empty());
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create port forwarding runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    let agent_path = runtime_dir.join(FORWARD_AGENT_NAME);
    fs::copy(current_exe()?, &agent_path).with_context(|| {
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
        cleanup_paths: vec![agent_path, runtime_dir.join(FORWARD_AGENT_SOCKET_NAME)],
    })
}

pub(crate) fn forward_agent_command() -> crate::docker::exec::ExecCommandSpec {
    crate::docker::exec::ExecCommandSpec {
        command: vec![FORWARD_AGENT_TARGET.to_owned()],
        user: None,
        working_dir: None,
        env: Default::default(),
        tty: false,
    }
}

pub(crate) async fn wait_for_forward_agent(runtime_dir: &Path) -> Result<PathBuf> {
    let socket_path = runtime_dir.join(FORWARD_AGENT_SOCKET_NAME);
    for _ in 0..FORWARD_AGENT_START_RETRIES {
        match UnixStream::connect(&socket_path).await {
            Ok(mut stream) => {
                stream.shutdown().await.ok();
                return Ok(socket_path);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                sleep(FORWARD_AGENT_START_DELAY).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to connect to port forwarding agent socket: {}",
                        socket_path.display()
                    )
                });
            }
        }
    }

    bail!(
        "Timed out waiting for port forwarding agent socket: {}",
        socket_path.display()
    )
}

pub(crate) async fn start_forward_session(
    forward_ports: &[ResolvedForwardPort],
    agent_socket_path: PathBuf,
) -> Result<ForwardSession> {
    let mut listeners = Vec::new();
    for port in forward_ports {
        if port.protocol != PortProtocol::Tcp {
            bail!("Unsupported port forwarding protocol: {:?}", port.protocol);
        }
        let host_ip = port
            .host_ip
            .parse::<IpAddr>()
            .with_context(|| format!("Invalid host IP for port forwarding: {}", port.host_ip))?;
        let listener = TcpListener::bind((host_ip, port.host))
            .await
            .with_context(|| {
                format!(
                    "Failed to bind port forwarding listener: {}:{}",
                    port.host_ip, port.host
                )
            })?;
        #[cfg(test)]
        let local_addr = listener.local_addr().with_context(|| {
            format!(
                "Failed to inspect port forwarding listener: {}:{}",
                port.host_ip, port.host
            )
        })?;
        let target_port = port.container;
        let socket_path = agent_socket_path.clone();
        let task = tokio::spawn(async move {
            run_forward_listener(listener, socket_path, target_port).await;
        });
        listeners.push(ForwardListener {
            task,
            #[cfg(test)]
            local_addr,
        });
    }

    Ok(ForwardSession {
        agent_socket_path,
        listeners,
    })
}

impl ForwardSession {
    #[cfg(test)]
    fn local_addr(&self, index: usize) -> std::net::SocketAddr {
        self.listeners[index].local_addr
    }

    pub(crate) async fn stop(mut self) {
        for listener in &self.listeners {
            listener.task.abort();
        }
        self.listeners.clear();
        let _ = send_agent_shutdown(&self.agent_socket_path).await;
    }
}

impl Drop for ForwardSession {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.task.abort();
        }
    }
}

pub(crate) async fn run_forward_agent() -> Result<()> {
    let socket_path = env::var("DECUNE_FORWARD_AGENT_SOCKET")
        .unwrap_or_else(|_| FORWARD_AGENT_SOCKET_TARGET.to_owned());
    run_forward_agent_at(Path::new(&socket_path)).await
}

async fn run_forward_listener(listener: TcpListener, agent_socket_path: PathBuf, target_port: u16) {
    while let Ok((client, _)) = listener.accept().await {
        let socket_path = agent_socket_path.clone();
        tokio::spawn(async move {
            let _ = proxy_client_connection(client, &socket_path, target_port).await;
        });
    }
}

async fn proxy_client_connection(
    mut client: TcpStream,
    agent_socket_path: &Path,
    target_port: u16,
) -> Result<()> {
    let mut agent = UnixStream::connect(agent_socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to port forwarding agent socket: {}",
                agent_socket_path.display()
            )
        })?;
    write_agent_request(
        &mut agent,
        ForwardAgentRequest {
            port: Some(target_port),
            shutdown: None,
        },
    )
    .await?;
    copy_bidirectional(&mut client, &mut agent)
        .await
        .context("Failed to proxy port forwarding stream")?;
    Ok(())
}

async fn run_forward_agent_at(socket_path: &Path) -> Result<()> {
    bind_forward_agent_socket(socket_path).await?;
    let listener = UnixListener::bind(socket_path).with_context(|| {
        format!(
            "Failed to bind port forwarding agent socket: {}",
            socket_path.display()
        )
    })?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o666)).with_context(|| {
        format!(
            "Failed to set port forwarding agent socket permissions: {}",
            socket_path.display()
        )
    })?;

    loop {
        let (stream, _) = listener.accept().await.with_context(|| {
            format!(
                "Failed to accept port forwarding agent connection: {}",
                socket_path.display()
            )
        })?;
        match read_agent_request(stream).await {
            Ok(AgentRequest::Forward { stream, port }) => {
                tokio::spawn(async move {
                    let _ = proxy_agent_connection(stream, port).await;
                });
            }
            Ok(AgentRequest::Shutdown) => break,
            Err(_) => {}
        }
    }

    remove_socket_file(socket_path).with_context(|| {
        format!(
            "Failed to remove port forwarding agent socket: {}",
            socket_path.display()
        )
    })
}

enum AgentRequest {
    Forward { stream: UnixStream, port: u16 },
    Shutdown,
}

async fn read_agent_request(stream: UnixStream) -> Result<AgentRequest> {
    let mut stream = stream;
    let line = read_agent_request_line(&mut stream).await?;
    let request: ForwardAgentRequest =
        serde_json::from_slice(&line).context("Invalid port forwarding agent request JSON")?;
    if request.shutdown.unwrap_or(false) {
        return Ok(AgentRequest::Shutdown);
    }
    let port = request
        .port
        .ok_or_else(|| anyhow::anyhow!("Port forwarding agent request is missing target port"))?;

    Ok(AgentRequest::Forward { stream, port })
}

async fn read_agent_request_line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0];
        let read = stream
            .read(&mut byte)
            .await
            .context("Failed to read port forwarding agent request")?;
        if read == 0 {
            bail!("Port forwarding agent request ended before newline");
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > 1024 {
            bail!("Port forwarding agent request exceeds 1024 bytes");
        }
    }
}

async fn proxy_agent_connection(mut stream: UnixStream, port: u16) -> Result<()> {
    let mut target = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("Failed to connect to container localhost port: {port}"))?;
    copy_bidirectional(&mut stream, &mut target)
        .await
        .context("Failed to proxy port forwarding agent stream")?;
    Ok(())
}

async fn bind_forward_agent_socket(socket_path: &Path) -> Result<()> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            remove_socket_file(socket_path).with_context(|| {
                format!(
                    "Failed to remove stale port forwarding agent socket: {}",
                    socket_path.display()
                )
            })?;
        }
        Ok(_) => {
            bail!(
                "Port forwarding agent socket path exists but is not a socket: {}",
                socket_path.display()
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect port forwarding agent socket: {}",
                    socket_path.display()
                )
            });
        }
    }

    Ok(())
}

async fn send_agent_shutdown(socket_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "Failed to connect to port forwarding agent socket for shutdown: {}",
            socket_path.display()
        )
    })?;
    write_agent_request(
        &mut stream,
        ForwardAgentRequest {
            port: None,
            shutdown: Some(true),
        },
    )
    .await
}

async fn write_agent_request(stream: &mut UnixStream, request: ForwardAgentRequest) -> Result<()> {
    let request =
        serde_json::to_vec(&request).context("Failed to serialize port forwarding request")?;
    stream
        .write_all(&request)
        .await
        .context("Failed to write port forwarding request")?;
    stream
        .write_all(b"\n")
        .await
        .context("Failed to finish port forwarding request")
}

fn current_exe() -> Result<PathBuf> {
    env::current_exe().context("Failed to locate current decune executable")
}

fn remove_socket_file(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener as StdTcpListener;

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn forwards_streams_through_agent_to_localhost_target() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let target_port = echo.local_addr().unwrap().port();
            let echo_task = tokio::spawn(async move {
                let (mut stream, _) = echo.accept().await.unwrap();
                let mut bytes = [0; 4];
                stream.read_exact(&mut bytes).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            });
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move { run_forward_agent_at(&agent_socket).await.unwrap() }
            });
            wait_for_socket(&agent_socket).await;
            let session =
                start_forward_session(&[forward_port(0, target_port)], agent_socket.clone())
                    .await
                    .unwrap();

            let mut client = TcpStream::connect(session.local_addr(0)).await.unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut response = [0; 4];
            client.read_exact(&mut response).await.unwrap();

            assert_eq!(&response, b"ping");
            session.stop().await;
            echo_task.await.unwrap();
            agent_task.await.unwrap();
            assert!(!agent_socket.exists());
        });
    }

    #[test]
    fn stop_closes_host_listener() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move { run_forward_agent_at(&agent_socket).await.unwrap() }
            });
            wait_for_socket(&agent_socket).await;
            let session = start_forward_session(&[forward_port(0, 9)], agent_socket)
                .await
                .unwrap();
            let local_addr = session.local_addr(0);

            session.stop().await;
            agent_task.await.unwrap();
            assert!(TcpStream::connect(local_addr).await.is_err());
        });
    }

    fn forward_port(host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            container,
            host,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }

    async fn wait_for_socket(socket_path: &Path) {
        for _ in 0..20 {
            if StdTcpListener::bind("127.0.0.1:0").is_ok()
                && UnixStream::connect(socket_path).await.is_ok()
            {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("socket did not become available");
    }
}

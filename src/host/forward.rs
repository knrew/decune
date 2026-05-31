use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    future::Future,
    io,
    io::Read as _,
    net::{IpAddr, Ipv6Addr},
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
    config::{
        resolved::{
            ResolvedAutoPorts, ResolvedConfig, ResolvedPortAttributes, ResolvedPublishPort,
        },
        types::{MountType, OnAutoForward, PortProtocol},
    },
    docker::ports::{ResolvedForwardPort, resolve_auto_forward_ports},
    host::credentials::DECUNE_RUNTIME_TARGET,
    host::runtime::set_private_runtime_parent,
    ui,
};

const FORWARD_AGENT_NAME: &str = "decune-forward-agent";
const FORWARD_AGENT_LINUX_X86_64_NAME: &str = "decune-forward-agent-linux-x86_64";
const FORWARD_AGENT_SOCKET_NAME: &str = "forward-agent.sock";
const FORWARD_AGENT_DIAGNOSTIC_NAME: &str = "forward-agent.err";
const FORWARD_AGENT_SOCKET_TARGET: &str = "/run/decune/forward-agent.sock";
const FORWARD_AGENT_TARGET: &str = "/run/decune/decune-forward-agent";
const FORWARD_AGENT_USER: &str = "0";
const FORWARD_AGENT_ALLOWED_PORTS_ENV: &str = "DECUNE_FORWARD_AGENT_ALLOWED_PORTS";
const FORWARD_AGENT_SECRET_ENV: &str = "DECUNE_FORWARD_AGENT_SECRET";
const FORWARD_AGENT_START_RETRIES: usize = 100;
const FORWARD_AGENT_START_DELAY: Duration = Duration::from_millis(50);
const FORWARD_AGENT_DIAGNOSTIC_TAIL_BYTES: usize = 4096;
const AUTO_FORWARD_INITIAL_DELAY: Duration = Duration::from_secs(3);
const AUTO_FORWARD_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const FORWARD_AGENT_LINUX_X86_64: &[u8] =
    include_bytes!("assets/decune-forward-agent-linux-x86_64");

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

#[derive(Debug)]
pub(crate) struct ForwardSession {
    agent_socket_path: PathBuf,
    secret: String,
    listeners: Vec<ForwardListener>,
    auto_task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct ForwardListener {
    task: JoinHandle<()>,
    #[cfg(test)]
    local_addr: std::net::SocketAddr,
}

impl Drop for ForwardListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardAgentRequest {
    port: Option<u16>,
    shutdown: Option<bool>,
    secret: Option<String>,
    scan: Option<ForwardAgentScanRequest>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardAgentScanRequest {
    min: u16,
    max: u16,
    ignore: Vec<u16>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardAgentScanResponse {
    ports: Vec<u16>,
}

#[derive(Debug, Clone)]
pub(crate) struct AutoForwardConfig {
    auto: ResolvedAutoPorts,
    publish_ports: Vec<ResolvedPublishPort>,
    port_attributes: BTreeMap<String, ResolvedPortAttributes>,
    other_ports_attributes: Option<ResolvedPortAttributes>,
}

impl AutoForwardConfig {
    pub(crate) fn from_config(config: &ResolvedConfig) -> Option<Self> {
        config.ports.auto.enabled.then(|| Self {
            auto: config.ports.auto.clone(),
            publish_ports: config.devcontainer.publish_ports.clone(),
            port_attributes: config.devcontainer.port_attributes.clone(),
            other_ports_attributes: config.devcontainer.other_ports_attributes.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardAgentStatus {
    Running,
    Exited { exit_code: Option<i64> },
}

#[derive(Debug, Clone)]
struct ForwardAgentAccess {
    allowed_ports: BTreeSet<u16>,
    secret: String,
}

impl ForwardAgentAccess {
    fn new(allowed_ports: impl IntoIterator<Item = u16>, secret: String) -> Self {
        Self {
            allowed_ports: allowed_ports.into_iter().collect(),
            secret,
        }
    }

    fn from_env() -> Result<Self> {
        let secret = env::var(FORWARD_AGENT_SECRET_ENV)
            .with_context(|| format!("Missing {FORWARD_AGENT_SECRET_ENV} for forward agent"))?;
        if secret.is_empty() {
            bail!("Forward agent secret is empty");
        }
        let allowed_ports = parse_allowed_ports_env(
            &env::var(FORWARD_AGENT_ALLOWED_PORTS_ENV).unwrap_or_default(),
        )?;
        Ok(Self::new(allowed_ports, secret))
    }

    fn allows_port(&self, port: u16) -> bool {
        self.allowed_ports.contains(&port)
    }

    fn secret_matches(&self, secret: Option<&str>) -> bool {
        secret == Some(self.secret.as_str())
    }
}

pub(crate) fn invoked_as_forward_agent() -> bool {
    matches!(
        env::args_os()
            .next()
            .as_deref()
            .map(Path::new)
            .and_then(Path::file_name)
            .and_then(|value| value.to_str()),
        Some(FORWARD_AGENT_NAME | FORWARD_AGENT_LINUX_X86_64_NAME)
    )
}

pub(crate) fn prepare_forward_runtime(
    _forward_ports: &[ResolvedForwardPort],
    runtime_dir: &Path,
) -> Result<ForwardRuntime> {
    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create port forwarding runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    set_private_runtime_parent(runtime_dir)?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set port forwarding runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;
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
    let linux_x86_64_agent_path = runtime_dir.join(FORWARD_AGENT_LINUX_X86_64_NAME);
    fs::write(&linux_x86_64_agent_path, FORWARD_AGENT_LINUX_X86_64).with_context(|| {
        format!(
            "Failed to stage Linux x86_64 port forwarding agent: {}",
            linux_x86_64_agent_path.display()
        )
    })?;
    fs::set_permissions(&linux_x86_64_agent_path, fs::Permissions::from_mode(0o755)).with_context(
        || {
            format!(
                "Failed to set Linux x86_64 port forwarding agent permissions: {}",
                linux_x86_64_agent_path.display()
            )
        },
    )?;

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
        cleanup_paths: vec![
            agent_path,
            linux_x86_64_agent_path,
            runtime_dir.join(FORWARD_AGENT_SOCKET_NAME),
            runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME),
        ],
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

#[cfg(test)]
async fn wait_for_forward_agent(runtime_dir: &Path) -> Result<PathBuf> {
    wait_for_forward_agent_with_status(runtime_dir, || async { Ok(ForwardAgentStatus::Running) })
        .await
}

pub(crate) async fn wait_for_forward_agent_with_status<F, Fut>(
    runtime_dir: &Path,
    mut agent_status: F,
) -> Result<PathBuf>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<ForwardAgentStatus>>,
{
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
                if let Some(error) = forward_agent_start_error(runtime_dir, agent_status().await?)?
                {
                    bail!("{error}");
                }
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

pub(crate) async fn start_forward_session_with_auto(
    forward_ports: &[ResolvedForwardPort],
    auto_forward: Option<AutoForwardConfig>,
    agent_socket_path: PathBuf,
    secret: String,
) -> Result<ForwardSession> {
    let listeners = match start_forward_listeners(forward_ports, &agent_socket_path, &secret).await
    {
        Ok(listeners) => listeners,
        Err(error) => {
            let _ = send_agent_shutdown(&agent_socket_path, &secret).await;
            return Err(error);
        }
    };
    let auto_task = auto_forward.map(|config| {
        tokio::spawn(run_auto_forward_loop(
            forward_ports.to_vec(),
            config,
            agent_socket_path.clone(),
            secret.clone(),
        ))
    });

    Ok(ForwardSession {
        agent_socket_path,
        secret,
        listeners,
        auto_task,
    })
}

async fn start_forward_listeners(
    forward_ports: &[ResolvedForwardPort],
    agent_socket_path: &Path,
    secret: &str,
) -> Result<Vec<ForwardListener>> {
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
        let socket_path = agent_socket_path.to_path_buf();
        let secret = secret.to_owned();
        let task = tokio::spawn(async move {
            run_forward_listener(listener, socket_path, target_port, secret).await;
        });
        listeners.push(ForwardListener {
            task,
            #[cfg(test)]
            local_addr,
        });
    }

    Ok(listeners)
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
        if let Some(task) = self.auto_task.take() {
            task.abort();
        }
        let _ = send_agent_shutdown(&self.agent_socket_path, &self.secret).await;
    }
}

impl Drop for ForwardSession {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.task.abort();
        }
        if let Some(task) = &self.auto_task {
            task.abort();
        }
    }
}

pub(crate) async fn run_forward_agent() -> Result<()> {
    let socket_path = env::var("DECUNE_FORWARD_AGENT_SOCKET")
        .unwrap_or_else(|_| FORWARD_AGENT_SOCKET_TARGET.to_owned());
    run_forward_agent_at_with_access(Path::new(&socket_path), ForwardAgentAccess::from_env()?).await
}

async fn run_forward_listener(
    listener: TcpListener,
    agent_socket_path: PathBuf,
    target_port: u16,
    secret: String,
) {
    while let Ok((client, _)) = listener.accept().await {
        let socket_path = agent_socket_path.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            let _ = proxy_client_connection(client, &socket_path, target_port, &secret).await;
        });
    }
}

async fn proxy_client_connection(
    mut client: TcpStream,
    agent_socket_path: &Path,
    target_port: u16,
    secret: &str,
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
            secret: Some(secret.to_owned()),
            scan: None,
        },
    )
    .await?;
    copy_bidirectional(&mut client, &mut agent)
        .await
        .context("Failed to proxy port forwarding stream")?;
    Ok(())
}

async fn run_forward_agent_at_with_access(
    socket_path: &Path,
    mut access: ForwardAgentAccess,
) -> Result<()> {
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
        match read_agent_request(stream, &access).await {
            Ok(AgentRequest::Forward { stream, port }) => {
                tokio::spawn(async move {
                    let _ = proxy_agent_connection(stream, port).await;
                });
            }
            Ok(AgentRequest::Scan { mut stream, scan }) => {
                if let Ok(ports) = detect_listen_ports(&scan) {
                    access.allowed_ports.extend(ports.iter().copied());
                    let _ = write_agent_scan_response(&mut stream, &ports).await;
                }
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
    Forward {
        stream: UnixStream,
        port: u16,
    },
    Scan {
        stream: UnixStream,
        scan: ForwardAgentScanRequest,
    },
    Shutdown,
}

async fn read_agent_request(
    stream: UnixStream,
    access: &ForwardAgentAccess,
) -> Result<AgentRequest> {
    let mut stream = stream;
    let line = read_agent_request_line(&mut stream).await?;
    let request: ForwardAgentRequest =
        serde_json::from_slice(&line).context("Invalid port forwarding agent request JSON")?;
    if !access.secret_matches(request.secret.as_deref()) {
        bail!("Port forwarding agent request is not authorized");
    }
    if request.shutdown.unwrap_or(false) {
        return Ok(AgentRequest::Shutdown);
    }
    if let Some(scan) = request.scan {
        return Ok(AgentRequest::Scan { stream, scan });
    }
    let port = request
        .port
        .ok_or_else(|| anyhow::anyhow!("Port forwarding agent request is missing target port"))?;
    if !access.allows_port(port) {
        bail!("Port forwarding agent request targets an unauthorized port: {port}");
    }

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

async fn run_auto_forward_loop(
    mut forward_ports: Vec<ResolvedForwardPort>,
    config: AutoForwardConfig,
    agent_socket_path: PathBuf,
    secret: String,
) {
    let mut listeners = Vec::new();
    let mut reported_error = false;
    sleep(AUTO_FORWARD_INITIAL_DELAY).await;

    loop {
        match scan_and_add_auto_forwards(
            &mut forward_ports,
            &mut listeners,
            &config,
            &agent_socket_path,
            &secret,
        )
        .await
        {
            Ok(()) => reported_error = false,
            Err(error) if !reported_error => {
                ui::warn(&format!("Automatic port forwarding failed: {error:#}"));
                reported_error = true;
            }
            Err(_) => {}
        }
        sleep(AUTO_FORWARD_SCAN_INTERVAL).await;
    }
}

async fn scan_and_add_auto_forwards(
    forward_ports: &mut Vec<ResolvedForwardPort>,
    listeners: &mut Vec<ForwardListener>,
    config: &AutoForwardConfig,
    agent_socket_path: &Path,
    secret: &str,
) -> Result<()> {
    let detected = request_auto_forward_ports(agent_socket_path, secret, &config.auto).await?;
    let additions = resolve_auto_forward_ports(
        detected,
        forward_ports,
        &config.publish_ports,
        &config.auto,
        &config.port_attributes,
        config.other_ports_attributes.as_ref(),
    )?;
    if additions.is_empty() {
        return Ok(());
    }

    let new_ports = additions
        .iter()
        .map(|addition| addition.port.clone())
        .collect::<Vec<_>>();
    let mut new_listeners = start_forward_listeners(&new_ports, agent_socket_path, secret).await?;
    for addition in additions {
        if addition.on_auto_forward == OnAutoForward::Notify {
            ui::info(&format!(
                "Forwarded localhost:{} -> container:{}",
                addition.port.host, addition.port.container
            ));
        }
        forward_ports.push(addition.port);
    }
    listeners.append(&mut new_listeners);

    Ok(())
}

async fn request_auto_forward_ports(
    agent_socket_path: &Path,
    secret: &str,
    auto: &ResolvedAutoPorts,
) -> Result<Vec<u16>> {
    let mut agent = UnixStream::connect(agent_socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to port forwarding agent socket for auto scan: {}",
                agent_socket_path.display()
            )
        })?;
    write_agent_request(
        &mut agent,
        ForwardAgentRequest {
            port: None,
            shutdown: None,
            secret: Some(secret.to_owned()),
            scan: Some(ForwardAgentScanRequest {
                min: auto.min,
                max: auto.max,
                ignore: auto.ignore.clone(),
            }),
        },
    )
    .await?;
    let mut response = Vec::new();
    agent
        .read_to_end(&mut response)
        .await
        .context("Failed to read automatic port forwarding scan response")?;
    let response: ForwardAgentScanResponse = serde_json::from_slice(&response)
        .context("Invalid automatic port forwarding scan response JSON")?;
    Ok(response.ports)
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

async fn send_agent_shutdown(socket_path: &Path, secret: &str) -> Result<()> {
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
            secret: Some(secret.to_owned()),
            scan: None,
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

async fn write_agent_scan_response(stream: &mut UnixStream, ports: &[u16]) -> Result<()> {
    let response = serde_json::to_vec(&ForwardAgentScanResponse {
        ports: ports.to_vec(),
    })
    .context("Failed to serialize automatic port forwarding scan response")?;
    stream
        .write_all(&response)
        .await
        .context("Failed to write automatic port forwarding scan response")
}

fn detect_listen_ports(scan: &ForwardAgentScanRequest) -> Result<Vec<u16>> {
    let tcp = fs::read_to_string("/proc/net/tcp")
        .context("Failed to read /proc/net/tcp for automatic port forwarding")?;
    let tcp6 = fs::read_to_string("/proc/net/tcp6")
        .context("Failed to read /proc/net/tcp6 for automatic port forwarding")?;
    listen_ports_from_proc_contents(
        tcp.as_str(),
        tcp6.as_str(),
        scan.min,
        scan.max,
        &scan.ignore,
    )
}

fn listen_ports_from_proc_contents(
    tcp_content: &str,
    tcp6_content: &str,
    min: u16,
    max: u16,
    ignore: &[u16],
) -> Result<Vec<u16>> {
    let ignored = ignore.iter().copied().collect::<BTreeSet<_>>();
    let mut ports = BTreeSet::new();

    for line in tcp_content.lines().skip(1) {
        let Some(port) = parse_proc_net_tcp_listen_port(line)? else {
            continue;
        };
        if port >= min && port < max && !ignored.contains(&port) {
            ports.insert(port);
        }
    }
    for line in tcp6_content.lines().skip(1) {
        let Some(port) = parse_proc_net_tcp6_listen_port(line)? else {
            continue;
        };
        if port >= min && port < max && !ignored.contains(&port) {
            ports.insert(port);
        }
    }

    Ok(ports.into_iter().collect())
}

fn parse_proc_net_tcp_listen_port(line: &str) -> Result<Option<u16>> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 4 || fields[3] != "0A" {
        return Ok(None);
    }
    let local_address = fields[1];
    let (_, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp local address: {local_address}"))?;
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp local port: {port_hex}"))?;

    Ok(Some(port))
}

fn parse_proc_net_tcp6_listen_port(line: &str) -> Result<Option<u16>> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 4 || fields[3] != "0A" {
        return Ok(None);
    }
    let local_address = fields[1];
    let (address_hex, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp6 local address: {local_address}"))?;
    if !proc_net_tcp6_address_is_ipv4_reachable(address_hex)? {
        return Ok(None);
    }
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp6 local port: {port_hex}"))?;

    Ok(Some(port))
}

fn proc_net_tcp6_address_is_ipv4_reachable(address_hex: &str) -> Result<bool> {
    let address = parse_proc_net_tcp6_address(address_hex)?;
    let Some(ipv4) = Ipv6Addr::from(address).to_ipv4_mapped() else {
        return Ok(false);
    };

    Ok(ipv4.is_loopback() || ipv4.is_unspecified())
}

fn parse_proc_net_tcp6_address(address_hex: &str) -> Result<[u8; 16]> {
    if address_hex.len() != 32 {
        bail!("Invalid /proc/net/tcp6 local address: {address_hex}");
    }
    let mut address = [0u8; 16];
    for chunk in 0..4 {
        let start = chunk * 8;
        let value = u32::from_str_radix(&address_hex[start..start + 8], 16)
            .with_context(|| format!("Invalid /proc/net/tcp6 local address: {address_hex}"))?;
        address[chunk * 4..chunk * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    Ok(address)
}

fn forward_agent_launcher() -> &'static [u8] {
    b"#!/bin/sh
set -eu
diag=\"/run/decune/forward-agent.err\"
: > \"$diag\" 2>/dev/null || true
arch=\"$(uname -m 2>/dev/null || true)\"
case \"$arch\" in
  x86_64|amd64)
    exec /run/decune/decune-forward-agent-linux-x86_64 \"$@\" 2>>\"$diag\"
    ;;
  *)
    message=\"Unsupported port forwarding agent container architecture: ${arch:-unknown}\"
    echo \"$message\" >&2
    echo \"$message\" >> \"$diag\" 2>/dev/null || true
    exit 1
    ;;
esac
"
}

fn forward_agent_start_error(
    runtime_dir: &Path,
    status: ForwardAgentStatus,
) -> Result<Option<String>> {
    let diagnostic = read_forward_agent_diagnostic(runtime_dir)?;
    if let ForwardAgentStatus::Exited { exit_code } = status {
        let exit_code = exit_code
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default();
        return Ok(Some(match diagnostic {
            Some(diagnostic) => format!(
                "Port forwarding agent exited before its socket became available{exit_code}. diagnostic: {diagnostic}"
            ),
            None => {
                format!(
                    "Port forwarding agent exited before its socket became available{exit_code}"
                )
            }
        }));
    }

    Ok(diagnostic.map(|diagnostic| format!("Port forwarding agent failed to start: {diagnostic}")))
}

fn read_forward_agent_diagnostic(runtime_dir: &Path) -> Result<Option<String>> {
    let path = runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read port forwarding agent diagnostic: {}",
                    path.display()
                )
            });
        }
    };
    let start = bytes
        .len()
        .saturating_sub(FORWARD_AGENT_DIAGNOSTIC_TAIL_BYTES);
    let diagnostic = String::from_utf8_lossy(&bytes[start..]).trim().to_owned();
    if diagnostic.is_empty() {
        return Ok(None);
    }

    Ok(Some(diagnostic))
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

fn parse_allowed_ports_env(value: &str) -> Result<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    for raw in value.split(',').filter(|part| !part.is_empty()) {
        let port = raw
            .parse::<u16>()
            .with_context(|| format!("Invalid forward agent allowed port: {raw}"))?;
        ports.insert(port);
    }
    Ok(ports)
}

#[cfg(test)]
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
    use std::{fs, net::TcpListener as StdTcpListener, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn proc_net_tcp_parser_detects_listen_ports_from_tcp_and_tcp6() {
        let tcp = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:10E1 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1 1 0000000000000000 100 0 0 10 0
   1: 0100007F:10E2 00000000:0000 01 00000000:00000000 00:00000000 00000000 0 0 2 1 0000000000000000 100 0 0 10 0
";
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:10E3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents(tcp, tcp6, 4321, 4324, &[4323]).unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn proc_net_tcp6_parser_ignores_ipv6_only_listen_ports() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, 4321, 4322, &[]).unwrap();

        assert!(ports.is_empty());
    }

    #[test]
    fn proc_net_tcp6_parser_detects_ipv4_mapped_loopback_listen_ports() {
        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:10E1 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 3 1 0000000000000000 100 0 0 10 0
";

        let ports = listen_ports_from_proc_contents("", tcp6, 4321, 4322, &[]).unwrap();

        assert_eq!(ports, vec![4321]);
    }

    #[test]
    fn runtime_stages_container_agent_even_without_forward_ports() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        let runtime = prepare_forward_runtime(&[], &runtime_dir).unwrap();

        assert!(runtime_dir.join("decune-forward-agent").is_file());
        assert!(
            runtime_dir
                .join("decune-forward-agent-linux-x86_64")
                .is_file()
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
        let spec = forward_agent_command(&[forward_port(54321, 4321)], "test-secret");

        assert_eq!(spec.user.as_deref(), Some("0"));
    }

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
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([target_port], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap()
                }
            });
            wait_for_socket(&agent_socket).await;
            let session = start_forward_session_with_auto(
                &[forward_port(0, target_port)],
                None,
                agent_socket.clone(),
                "test-secret".to_owned(),
            )
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
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([9], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap()
                }
            });
            wait_for_socket(&agent_socket).await;
            let session = start_forward_session_with_auto(
                &[forward_port(0, 9)],
                None,
                agent_socket,
                "test-secret".to_owned(),
            )
            .await
            .unwrap();
            let local_addr = session.local_addr(0);

            session.stop().await;
            agent_task.await.unwrap();
            assert!(TcpStream::connect(local_addr).await.is_err());
        });
    }

    #[test]
    fn wait_for_forward_agent_reports_agent_diagnostic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            fs::write(
                temp.path().join("forward-agent.err"),
                "Unsupported port forwarding agent container architecture: aarch64\n",
            )
            .unwrap();

            let error = wait_for_forward_agent(temp.path()).await.unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("Unsupported port forwarding agent container architecture"));
            assert!(!message.contains("Timed out waiting"));
        });
    }

    #[test]
    fn listener_start_failure_stops_agent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let occupied_port = occupied.local_addr().unwrap().port();
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([9], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap()
                }
            });
            wait_for_socket(&agent_socket).await;

            let error = start_forward_session_with_auto(
                &[forward_port(occupied_port, 9)],
                None,
                agent_socket.clone(),
                "test-secret".to_owned(),
            )
            .await
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("Failed to bind port forwarding listener"));
            timeout(Duration::from_secs(1), agent_task)
                .await
                .unwrap()
                .unwrap();
            assert!(!agent_socket.exists());
        });
    }

    #[test]
    fn agent_rejects_unauthorized_port_and_shutdown_requests() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let agent_socket = temp.path().join("forward-agent.sock");
            let agent_task = tokio::spawn({
                let agent_socket = agent_socket.clone();
                async move {
                    run_forward_agent_at_with_access(
                        &agent_socket,
                        ForwardAgentAccess::new([4321], "test-secret".to_owned()),
                    )
                    .await
                    .unwrap()
                }
            });
            wait_for_socket(&agent_socket).await;

            send_raw_agent_request(
                &agent_socket,
                br#"{"port":4321,"shutdown":null,"secret":"wrong"}"#,
            )
            .await;
            assert!(agent_socket.exists());

            send_raw_agent_request(
                &agent_socket,
                br#"{"port":5432,"shutdown":null,"secret":"test-secret"}"#,
            )
            .await;
            assert!(agent_socket.exists());

            send_raw_agent_request(
                &agent_socket,
                br#"{"port":null,"shutdown":true,"secret":"wrong"}"#,
            )
            .await;
            assert!(agent_socket.exists());

            send_raw_agent_request(
                &agent_socket,
                br#"{"port":null,"shutdown":true,"secret":"test-secret"}"#,
            )
            .await;
            agent_task.await.unwrap();
            assert!(!agent_socket.exists());
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

    async fn send_raw_agent_request(socket_path: &Path, request: &[u8]) {
        let mut stream = UnixStream::connect(socket_path).await.unwrap();
        stream.write_all(request).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

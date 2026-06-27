#![allow(
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
    clippy::similar_names,
    clippy::string_slice,
    clippy::unused_async,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

use std::{
    collections::BTreeSet,
    env, fs, io,
    net::{Ipv4Addr, Ipv6Addr},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result, bail};
use decune_container_protocol::{
    ForwardAgentRequest, ForwardAgentScanRequest, ForwardAgentScanResponse,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
};

const FORWARD_AGENT_SOCKET_TARGET: &str = "/run/decune/forward-agent.sock";
const FORWARD_AGENT_ALLOWED_PORTS_ENV: &str = "DECUNE_FORWARD_AGENT_ALLOWED_PORTS";
const FORWARD_AGENT_SECRET_ENV: &str = "DECUNE_FORWARD_AGENT_SECRET";

fn main() {
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to initialize async runtime")
        .and_then(|runtime| runtime.block_on(run()));
    if let Err(error) = result {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let socket_path = env::var("DECUNE_FORWARD_AGENT_SOCKET")
        .unwrap_or_else(|_| FORWARD_AGENT_SOCKET_TARGET.to_owned());
    run_forward_agent_at_with_access(Path::new(&socket_path), ForwardAgentAccess::from_env()?).await
}

#[derive(Debug, Clone)]
struct ForwardAgentAccess {
    allowed_ports: BTreeSet<u16>,
    secret: String,
}

impl ForwardAgentAccess {
    fn from_env() -> Result<Self> {
        let secret = env::var(FORWARD_AGENT_SECRET_ENV)
            .with_context(|| format!("Missing {FORWARD_AGENT_SECRET_ENV} for forward agent"))?;
        if secret.is_empty() {
            bail!("Forward agent secret is empty");
        }
        let allowed_ports = parse_allowed_ports_env(
            &env::var(FORWARD_AGENT_ALLOWED_PORTS_ENV).unwrap_or_default(),
        )?;
        Ok(Self {
            allowed_ports: allowed_ports.into_iter().collect(),
            secret,
        })
    }

    fn allows_port(&self, port: u16) -> bool {
        self.allowed_ports.contains(&port)
    }

    fn secret_matches(&self, secret: Option<&str>) -> bool {
        secret == Some(self.secret.as_str())
    }
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

async fn proxy_agent_connection(mut stream: UnixStream, port: u16) -> Result<()> {
    let mut target = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .with_context(|| format!("Failed to connect to container localhost port: {port}"))?;
    copy_bidirectional(&mut stream, &mut target)
        .await
        .context("Failed to proxy port forwarding stream")?;
    Ok(())
}

async fn bind_forward_agent_socket(socket_path: &Path) -> Result<()> {
    match fs::metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            remove_socket_file(socket_path).with_context(|| {
                format!(
                    "Failed to remove stale port forwarding agent socket: {}",
                    socket_path.display()
                )
            })?;
        }
        Ok(_) => bail!(
            "Port forwarding agent socket path exists and is not a socket: {}",
            socket_path.display()
        ),
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

fn remove_socket_file(socket_path: &Path) -> Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove port forwarding agent socket: {}",
                socket_path.display()
            )
        }),
    }
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
    detect_listen_ports_from_proc_paths(
        scan,
        Path::new("/proc/net/tcp"),
        Path::new("/proc/net/tcp6"),
        Path::new("/proc/sys/net/ipv6/bindv6only"),
    )
}

fn detect_listen_ports_from_proc_paths(
    scan: &ForwardAgentScanRequest,
    tcp_path: &Path,
    tcp6_path: &Path,
    bindv6only_path: &Path,
) -> Result<Vec<u16>> {
    let tcp = read_required_proc_file(tcp_path)?;
    let tcp6 = read_proc_file(tcp6_path)?.unwrap_or_default();
    let tcp6_dual_stack =
        read_ipv6_bindv6only(bindv6only_path)?.is_some_and(|bindv6only| !bindv6only);
    listen_ports_from_proc_contents(
        tcp.as_str(),
        tcp6.as_str(),
        tcp6_dual_stack,
        scan.min,
        scan.max,
        &scan.ignore,
    )
}

fn read_proc_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read {} for automatic port forwarding",
                path.display()
            )
        }),
    }
}

fn read_required_proc_file(path: &Path) -> Result<String> {
    read_proc_file(path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to read {} for automatic port forwarding: file does not exist",
            path.display()
        )
    })
}

fn read_ipv6_bindv6only(path: &Path) -> Result<Option<bool>> {
    let Some(value) = read_proc_file(path)? else {
        return Ok(None);
    };
    match value.trim() {
        "0" => Ok(Some(false)),
        "1" => Ok(Some(true)),
        value => bail!("Invalid /proc/sys/net/ipv6/bindv6only value: {value}"),
    }
}

fn listen_ports_from_proc_contents(
    tcp_content: &str,
    tcp6_content: &str,
    tcp6_dual_stack: bool,
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
        let Some(port) = parse_proc_net_tcp6_listen_port(line, tcp6_dual_stack)? else {
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
    let (address_hex, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp local address: {local_address}"))?;
    if !proc_net_tcp_address_is_ipv4_reachable(address_hex)? {
        return Ok(None);
    }
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp local port: {port_hex}"))?;

    Ok(Some(port))
}

fn proc_net_tcp_address_is_ipv4_reachable(address_hex: &str) -> Result<bool> {
    let address = parse_proc_net_tcp_address(address_hex)?;

    Ok(address == Ipv4Addr::LOCALHOST || address.is_unspecified())
}

fn parse_proc_net_tcp_address(address_hex: &str) -> Result<Ipv4Addr> {
    if address_hex.len() != 8 {
        bail!("Invalid /proc/net/tcp local address: {address_hex}");
    }
    let address = u32::from_str_radix(address_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp local address: {address_hex}"))?;

    Ok(Ipv4Addr::from(address.to_le_bytes()))
}

fn parse_proc_net_tcp6_listen_port(line: &str, tcp6_dual_stack: bool) -> Result<Option<u16>> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 4 || fields[3] != "0A" {
        return Ok(None);
    }
    let local_address = fields[1];
    let (address_hex, port_hex) = local_address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid /proc/net/tcp6 local address: {local_address}"))?;
    if !proc_net_tcp6_address_is_ipv4_reachable(address_hex, tcp6_dual_stack)? {
        return Ok(None);
    }
    let port = u16::from_str_radix(port_hex, 16)
        .with_context(|| format!("Invalid /proc/net/tcp6 local port: {port_hex}"))?;

    Ok(Some(port))
}

fn proc_net_tcp6_address_is_ipv4_reachable(
    address_hex: &str,
    tcp6_dual_stack: bool,
) -> Result<bool> {
    let address = parse_proc_net_tcp6_address(address_hex)?;
    let ipv6 = Ipv6Addr::from(address);
    if ipv6.is_unspecified() {
        return Ok(tcp6_dual_stack);
    }
    let Some(ipv4) = ipv6.to_ipv4_mapped() else {
        return Ok(false);
    };

    Ok(ipv4 == Ipv4Addr::LOCALHOST || ipv4.is_unspecified())
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

fn parse_allowed_ports_env(value: &str) -> Result<Vec<u16>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|port| {
            let port = port.trim();
            port.parse::<u16>()
                .with_context(|| format!("Invalid allowed forward agent port: {port}"))
        })
        .collect()
}

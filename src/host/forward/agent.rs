use std::{
    collections::BTreeSet,
    env, fs,
    future::Future,
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use decune_container_protocol::{
    ForwardAgentRequest, ForwardAgentScanRequest, ForwardAgentScanResponse,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpStream, UnixListener, UnixStream},
    time::sleep,
};

use super::{
    FORWARD_AGENT_ALLOWED_PORTS_ENV, FORWARD_AGENT_DIAGNOSTIC_NAME,
    FORWARD_AGENT_DIAGNOSTIC_TAIL_BYTES, FORWARD_AGENT_NAME, FORWARD_AGENT_SECRET_ENV,
    FORWARD_AGENT_SOCKET_NAME, FORWARD_AGENT_SOCKET_TARGET, FORWARD_AGENT_START_DELAY,
    FORWARD_AGENT_START_RETRIES, FORWARD_AGENT_STATUS_NAME, proc_scan::detect_listen_ports,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardAgentStatus {
    Running,
    Exited { exit_code: Option<i64> },
}

#[derive(Debug, Clone)]
pub(super) struct ForwardAgentAccess {
    allowed_ports: BTreeSet<u16>,
    secret: String,
}

impl ForwardAgentAccess {
    pub(super) fn new(allowed_ports: impl IntoIterator<Item = u16>, secret: String) -> Self {
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
    env::args_os()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        == Some(FORWARD_AGENT_NAME)
}

pub(crate) async fn run_forward_agent() -> Result<()> {
    let socket_path = env::var("DECUNE_FORWARD_AGENT_SOCKET")
        .unwrap_or_else(|_| FORWARD_AGENT_SOCKET_TARGET.to_owned());
    let socket_path = Path::new(&socket_path);
    let result = async {
        let access = ForwardAgentAccess::from_env()?;
        run_forward_agent_at_with_access(socket_path, access).await
    }
    .await;
    if let Err(error) = &result {
        write_forward_agent_failure(socket_path, error);
    }
    result
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
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::PermissionDenied
                ) =>
            {
                let status = match read_forward_agent_status(runtime_dir)? {
                    Some(status) => status,
                    None => agent_status().await?,
                };
                if let Some(error) = forward_agent_start_error(runtime_dir, status)? {
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

pub(super) async fn run_forward_agent_at_with_access(
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

pub(super) async fn send_agent_shutdown(socket_path: &Path, secret: &str) -> Result<()> {
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

pub(super) async fn write_agent_request(
    stream: &mut UnixStream,
    request: ForwardAgentRequest,
) -> Result<()> {
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

fn write_forward_agent_failure(socket_path: &Path, error: &anyhow::Error) {
    let Some(runtime_dir) = socket_path.parent() else {
        return;
    };
    let _ = fs::write(
        runtime_dir.join(FORWARD_AGENT_DIAGNOSTIC_NAME),
        format!("{error:#}\n"),
    );
    let _ = fs::write(runtime_dir.join(FORWARD_AGENT_STATUS_NAME), "exited\n");
}

fn read_forward_agent_status(runtime_dir: &Path) -> Result<Option<ForwardAgentStatus>> {
    let path = runtime_dir.join(FORWARD_AGENT_STATUS_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read port forwarding agent status: {}",
                    path.display()
                )
            });
        }
    };
    let status = String::from_utf8_lossy(&bytes);
    let status = status.trim();
    if status == "exited" {
        return Ok(Some(ForwardAgentStatus::Exited { exit_code: None }));
    }
    if let Some(exit_code) = status.strip_prefix("exited:") {
        return Ok(Some(ForwardAgentStatus::Exited {
            exit_code: exit_code.trim().parse().ok(),
        }));
    }
    Ok(None)
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

fn remove_socket_file(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::{fs, net::TcpListener as StdTcpListener, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    use super::*;

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
                "Unsupported port forwarding agent container architecture: riscv64\n",
            )
            .unwrap();

            let error = wait_for_forward_agent(temp.path()).await.unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("Unsupported port forwarding agent container architecture"));
            assert!(!message.contains("Timed out waiting"));
        });
    }

    #[test]
    fn wait_for_forward_agent_reports_agent_status_exit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            fs::write(temp.path().join(FORWARD_AGENT_STATUS_NAME), "exited\n").unwrap();
            fs::write(
                temp.path().join(FORWARD_AGENT_DIAGNOSTIC_NAME),
                "startup failed\n",
            )
            .unwrap();

            let error = wait_for_forward_agent_with_status(temp.path(), || async {
                Ok(ForwardAgentStatus::Running)
            })
            .await
            .unwrap_err();
            let message = format!("{error:#}");

            assert!(message.contains("Port forwarding agent exited"));
            assert!(message.contains("startup failed"));
            assert!(!message.contains("Timed out waiting"));
        });
    }

    #[test]
    fn wait_for_forward_agent_retries_socket_permission_denied() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let socket_path = temp.path().join(FORWARD_AGENT_SOCKET_NAME);
            let listener = UnixListener::bind(&socket_path).unwrap();
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o000)).unwrap();
            let chmod_task = tokio::spawn({
                let socket_path = socket_path.clone();
                async move {
                    sleep(super::FORWARD_AGENT_START_DELAY * 2).await;
                    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666)).unwrap();
                }
            });
            let accept_task = tokio::spawn(async move {
                let _ = listener.accept().await.unwrap();
            });

            let ready = wait_for_forward_agent_with_status(temp.path(), || async {
                Ok(ForwardAgentStatus::Running)
            })
            .await
            .unwrap();

            assert_eq!(ready, socket_path);
            chmod_task.await.unwrap();
            accept_task.await.unwrap();
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
            timeout(std::time::Duration::from_secs(1), agent_task)
                .await
                .unwrap()
                .unwrap();
            assert!(!agent_socket.exists());
        });
    }

    pub(crate) async fn wait_for_socket(socket_path: &Path) {
        for _ in 0..20 {
            if StdTcpListener::bind("127.0.0.1:0").is_ok()
                && UnixStream::connect(socket_path).await.is_ok()
            {
                return;
            }
            sleep(std::time::Duration::from_millis(25)).await;
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
}

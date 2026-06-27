use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

use crate::{
    config::types::PortProtocol, docker::ports::ResolvedForwardPort,
    host::runtime::prepare_private_runtime_dir,
};

use super::runtime::new_forward_agent_socket_id;

const FORWARD_STATUS_PROTOCOL_VERSION: u16 = 1;
const FORWARD_STATUS_FILE_PREFIX: &str = "forward-status-";
const FORWARD_STATUS_SOCKET_SUFFIX: &str = ".sock";
const FORWARD_STATUS_METADATA_SUFFIX: &str = ".json";
const FORWARD_STATUS_REQUEST_TYPE_LIST: &str = "list";
const FORWARD_STATUS_DIR_SUFFIX: &str = "-ports";

#[derive(Debug)]
pub(crate) struct ForwardStatusServer {
    registry: ForwardStatusRegistry,
    socket_path: PathBuf,
    metadata_path: PathBuf,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ForwardStatusRegistry {
    ports: Arc<Mutex<Vec<ActiveForwardPort>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveForwardPort {
    pub(crate) host_ip: String,
    pub(crate) host_port: u16,
    pub(crate) requested_host_port: u16,
    pub(crate) service: Option<String>,
    pub(crate) container_port: u16,
    pub(crate) protocol: String,
    pub(crate) source: ForwardStatusSource,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ForwardStatusSource {
    Configured,
    Auto,
}

impl ForwardStatusSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardStatusList {
    pub(crate) ports: Vec<ActiveForwardPort>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardStatusMetadata {
    version: u16,
    session_id: String,
    socket_name: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardStatusRequest {
    version: u16,
    #[serde(rename = "type")]
    request_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForwardStatusResponse {
    version: u16,
    ports: Vec<ActiveForwardPort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ForwardStatusRegistry {
    pub(crate) fn record(&self, port: &ResolvedForwardPort, source: ForwardStatusSource) {
        let mut ports = self
            .ports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ports.push(active_forward_port(port, source));
    }

    fn list(&self) -> Vec<ActiveForwardPort> {
        self.ports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) async fn start_forward_status_server(
    status_dir: impl AsRef<Path>,
) -> Result<ForwardStatusServer> {
    let status_dir = status_dir.as_ref();
    prepare_private_runtime_dir(status_dir, "port forwarding status")?;
    let session_id = new_forward_agent_socket_id()?;
    let socket_name = forward_status_socket_name(&session_id);
    let metadata_name = forward_status_metadata_name(&session_id);
    let socket_path = status_dir.join(&socket_name);
    let metadata_path = status_dir.join(&metadata_name);
    remove_file_if_exists(&socket_path)?;
    remove_file_if_exists(&metadata_path)?;

    let listener = UnixListener::bind(&socket_path).with_context(|| {
        format!(
            "Failed to bind port forwarding status socket: {}",
            socket_path.display()
        )
    })?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Failed to set port forwarding status socket permissions: {}",
            socket_path.display()
        )
    })?;
    write_forward_status_metadata(
        &metadata_path,
        &ForwardStatusMetadata {
            version: FORWARD_STATUS_PROTOCOL_VERSION,
            session_id,
            socket_name,
            pid: std::process::id(),
        },
    )?;

    let registry = ForwardStatusRegistry::default();
    let task = tokio::spawn(run_forward_status_server(listener, registry.clone()));

    Ok(ForwardStatusServer {
        registry,
        socket_path,
        metadata_path,
        task: Some(task),
    })
}

impl ForwardStatusServer {
    pub(crate) fn registry(&self) -> ForwardStatusRegistry {
        self.registry.clone()
    }

    pub(crate) async fn stop(mut self) {
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            _ = task.await;
        }
        cleanup_status_files(&self.metadata_path, &self.socket_path);
    }
}

impl Drop for ForwardStatusServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        cleanup_status_files(&self.metadata_path, &self.socket_path);
    }
}

pub(crate) fn forward_status_dir(runtime_dir: &Path) -> PathBuf {
    let name = runtime_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    runtime_dir.parent().map_or_else(
        || PathBuf::from(format!("{name}{FORWARD_STATUS_DIR_SUFFIX}")),
        |parent| parent.join(format!("{name}{FORWARD_STATUS_DIR_SUFFIX}")),
    )
}

pub(crate) async fn list_active_forward_status_ports(
    status_dir: impl AsRef<Path>,
) -> Result<ForwardStatusList> {
    let status_dir = status_dir.as_ref();
    let entries = match fs::read_dir(status_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ForwardStatusList {
                ports: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read port forwarding status directory: {}",
                    status_dir.display()
                )
            });
        }
    };

    let mut metadata_paths = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_forward_status_metadata_path(path))
        .collect::<Vec<_>>();
    metadata_paths.sort();

    let mut ports = Vec::new();
    let mut warnings = Vec::new();
    for metadata_path in metadata_paths {
        let metadata = match read_forward_status_metadata(&metadata_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "Ignoring invalid port forwarding status metadata {}: {error:#}",
                    metadata_path.display()
                ));
                continue;
            }
        };
        if metadata.version != FORWARD_STATUS_PROTOCOL_VERSION {
            warnings.push(format!(
                "Ignoring unsupported port forwarding status metadata version {} in {}",
                metadata.version,
                metadata_path.display()
            ));
            continue;
        }
        if !is_plain_file_name(&metadata.socket_name) {
            warnings.push(format!(
                "Ignoring unsafe port forwarding status socket name in {}",
                metadata_path.display()
            ));
            continue;
        }

        let socket_path = status_dir.join(&metadata.socket_name);
        match query_forward_status_socket(&socket_path).await {
            Ok(mut active) => ports.append(&mut active),
            Err(error) if is_stale_status_socket_error(&error) => {}
            Err(error) => warnings.push(format!(
                "Failed to query port forwarding status socket {}: {error:#}",
                socket_path.display()
            )),
        }
    }

    Ok(ForwardStatusList { ports, warnings })
}

pub(crate) fn remove_forward_status_dir(status_dir: impl AsRef<Path>) -> Result<()> {
    match fs::remove_dir_all(status_dir.as_ref()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to remove port forwarding status directory: {}",
                status_dir.as_ref().display()
            )
        }),
    }
}

async fn run_forward_status_server(listener: UnixListener, registry: ForwardStatusRegistry) {
    while let Ok((stream, _)) = listener.accept().await {
        let registry = registry.clone();
        tokio::spawn(async move {
            _ = handle_forward_status_connection(stream, registry).await;
        });
    }
}

async fn handle_forward_status_connection(
    mut stream: UnixStream,
    registry: ForwardStatusRegistry,
) -> Result<()> {
    let mut request = Vec::new();
    stream
        .read_to_end(&mut request)
        .await
        .context("Failed to read port forwarding status request")?;
    let response = handle_forward_status_request(&request, &registry);
    let response =
        serde_json::to_vec(&response).context("Failed to serialize port forwarding status")?;
    stream
        .write_all(&response)
        .await
        .context("Failed to write port forwarding status response")?;
    stream
        .shutdown()
        .await
        .context("Failed to finish port forwarding status response")?;
    Ok(())
}

fn handle_forward_status_request(
    request: &[u8],
    registry: &ForwardStatusRegistry,
) -> ForwardStatusResponse {
    let request = match serde_json::from_slice::<ForwardStatusRequest>(request) {
        Ok(request) => request,
        Err(error) => return ForwardStatusResponse::error(format!("Invalid request: {error}")),
    };
    if request.version != FORWARD_STATUS_PROTOCOL_VERSION {
        return ForwardStatusResponse::error(format!(
            "Unsupported protocol version: {}",
            request.version
        ));
    }
    if request.request_type != FORWARD_STATUS_REQUEST_TYPE_LIST {
        return ForwardStatusResponse::error(format!(
            "Unknown request type: {}",
            request.request_type
        ));
    }

    ForwardStatusResponse {
        version: FORWARD_STATUS_PROTOCOL_VERSION,
        ports: registry.list(),
        error: None,
    }
}

impl ForwardStatusResponse {
    const fn error(error: String) -> Self {
        Self {
            version: FORWARD_STATUS_PROTOCOL_VERSION,
            ports: Vec::new(),
            error: Some(error),
        }
    }
}

async fn query_forward_status_socket(socket_path: &Path) -> Result<Vec<ActiveForwardPort>> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "Failed to connect to port forwarding status socket: {}",
            socket_path.display()
        )
    })?;
    let request = serde_json::to_vec(&ForwardStatusRequest {
        version: FORWARD_STATUS_PROTOCOL_VERSION,
        request_type: FORWARD_STATUS_REQUEST_TYPE_LIST.to_owned(),
    })
    .context("Failed to serialize port forwarding status request")?;
    stream
        .write_all(&request)
        .await
        .context("Failed to write port forwarding status request")?;
    stream
        .shutdown()
        .await
        .context("Failed to finish port forwarding status request")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("Failed to read port forwarding status response")?;
    let response: ForwardStatusResponse = serde_json::from_slice(&response)
        .context("Invalid port forwarding status response JSON")?;
    if response.version != FORWARD_STATUS_PROTOCOL_VERSION {
        bail!(
            "Unsupported port forwarding status protocol version: {}",
            response.version
        );
    }
    if let Some(error) = response.error {
        bail!("Port forwarding status request failed: {error}");
    }

    Ok(response.ports)
}

fn active_forward_port(
    port: &ResolvedForwardPort,
    source: ForwardStatusSource,
) -> ActiveForwardPort {
    ActiveForwardPort {
        host_ip: port.host_ip.clone(),
        host_port: port.host,
        requested_host_port: port.requested_host,
        service: port.service.clone(),
        container_port: port.container,
        protocol: protocol_name(port.protocol).to_owned(),
        source,
        label: port.label.clone(),
    }
}

const fn protocol_name(protocol: PortProtocol) -> &'static str {
    match protocol {
        PortProtocol::Tcp => "tcp",
        PortProtocol::Udp => "udp",
    }
}

fn forward_status_socket_name(session_id: &str) -> String {
    format!("{FORWARD_STATUS_FILE_PREFIX}{session_id}{FORWARD_STATUS_SOCKET_SUFFIX}")
}

fn forward_status_metadata_name(session_id: &str) -> String {
    format!("{FORWARD_STATUS_FILE_PREFIX}{session_id}{FORWARD_STATUS_METADATA_SUFFIX}")
}

fn write_forward_status_metadata(path: &Path, metadata: &ForwardStatusMetadata) -> Result<()> {
    let content = serde_json::to_vec(metadata)
        .context("Failed to serialize port forwarding status metadata")?;
    fs::write(path, content).with_context(|| {
        format!(
            "Failed to write port forwarding status metadata: {}",
            path.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Failed to set port forwarding status metadata permissions: {}",
            path.display()
        )
    })
}

fn read_forward_status_metadata(path: &Path) -> Result<ForwardStatusMetadata> {
    let content = fs::read(path).with_context(|| {
        format!(
            "Failed to read port forwarding status metadata: {}",
            path.display()
        )
    })?;
    serde_json::from_slice(&content).with_context(|| {
        format!(
            "Invalid port forwarding status metadata JSON: {}",
            path.display()
        )
    })
}

fn is_forward_status_metadata_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(FORWARD_STATUS_FILE_PREFIX)
                && name.ends_with(FORWARD_STATUS_METADATA_SUFFIX)
        })
}

fn is_plain_file_name(value: &str) -> bool {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == value)
}

fn is_stale_status_socket_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|error| error.downcast_ref::<io::Error>())
        .any(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
        })
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove stale file: {}", path.display()))
        }
    }
}

fn cleanup_status_files(metadata_path: &Path, socket_path: &Path) {
    _ = fs::remove_file(metadata_path);
    _ = fs::remove_file(socket_path);
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn forward_port(host: u16, requested_host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            service: None,
            container,
            requested_host,
            host,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: Some("web".to_owned()),
        }
    }

    #[test]
    fn status_server_lists_recorded_ports() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let temp = TempDir::new().unwrap();
            let server = start_forward_status_server(temp.path()).await.unwrap();
            let registry = server.registry();
            registry.record(
                &forward_port(3001, 3000, 3000),
                ForwardStatusSource::Configured,
            );
            let mut sidecar = forward_port(5433, 5432, 5432);
            sidecar.service = Some("db".to_owned());
            sidecar.label = None;
            registry.record(&sidecar, ForwardStatusSource::Auto);

            let list = list_active_forward_status_ports(temp.path()).await.unwrap();

            assert!(list.warnings.is_empty(), "{:?}", list.warnings);
            assert_eq!(
                list.ports,
                vec![
                    ActiveForwardPort {
                        host_ip: "127.0.0.1".to_owned(),
                        host_port: 3001,
                        requested_host_port: 3000,
                        service: None,
                        container_port: 3000,
                        protocol: "tcp".to_owned(),
                        source: ForwardStatusSource::Configured,
                        label: Some("web".to_owned()),
                    },
                    ActiveForwardPort {
                        host_ip: "127.0.0.1".to_owned(),
                        host_port: 5433,
                        requested_host_port: 5432,
                        service: Some("db".to_owned()),
                        container_port: 5432,
                        protocol: "tcp".to_owned(),
                        source: ForwardStatusSource::Auto,
                        label: None,
                    },
                ]
            );

            server.stop().await;
        });
    }

    #[test]
    fn missing_status_directory_lists_no_ports() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let temp = TempDir::new().unwrap();
            let list = list_active_forward_status_ports(temp.path().join("missing"))
                .await
                .unwrap();

            assert!(list.ports.is_empty());
            assert!(list.warnings.is_empty());
        });
    }

    #[test]
    fn stale_status_metadata_is_ignored_without_removing_files() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let temp = TempDir::new().unwrap();
            let metadata_path = temp.path().join(forward_status_metadata_name("stale"));
            let socket_name = forward_status_socket_name("stale");
            let socket_path = temp.path().join(&socket_name);
            let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
            drop(listener);
            write_forward_status_metadata(
                &metadata_path,
                &ForwardStatusMetadata {
                    version: FORWARD_STATUS_PROTOCOL_VERSION,
                    session_id: "stale".to_owned(),
                    socket_name: socket_name.clone(),
                    pid: 1,
                },
            )
            .unwrap();

            let list = list_active_forward_status_ports(temp.path()).await.unwrap();

            assert!(list.ports.is_empty());
            assert!(list.warnings.is_empty());
            assert!(metadata_path.exists());
            assert!(socket_path.exists());
        });
    }
}

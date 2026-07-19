use std::{
    error::Error,
    fmt, fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use decune_container_protocol::{ERROR_CODE_CLI_QUERY_FAILED, ERROR_CODE_REQUEST_TOO_LARGE};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinHandle, JoinSet},
};

use crate::{
    config::types::GitHttpsMode,
    host::{
        credentials::{GitCredentialExecutor, SystemGitCredentialExecutor},
        protocol::{HostDaemonRequestDispatch, HostDaemonResponse, handle_host_daemon_request},
        query::ContainerCliQueryService,
        query_context::{HostDaemonCliQueryIdentity, HostDaemonCliQueryPolicy},
        runtime::{
            create_runtime_dir, set_private_runtime_parent, set_runtime_dir_mode,
            validate_runtime_dir_mode,
        },
    },
};

const HOST_DAEMON_SOCKET_NAME: &str = "host-daemon.sock";
const HOST_DAEMON_METADATA_NAME: &str = "host-daemon.json";
const MAX_HOST_DAEMON_REQUEST_BYTES: usize = 64 * 1024;
const ACTIVE_HOST_DAEMON_CONNECTIONS: usize = 32;
const HOST_DAEMON_REQUEST_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const HOST_DAEMON_RESPONSE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const HOST_DAEMON_QUERY_IDENTITY_MISMATCH: &str = "An active decune up session uses a different container CLI policy or query context; stop all decune up sessions for this workspace and retry";
pub(crate) const HOST_DAEMON_VERSION_MISMATCH: &str = "An active decune up session uses an incompatible host daemon metadata or protocol version, possibly from a different decune version; stop all decune up sessions for this workspace and retry";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostDaemonAccess {
    runtime_dir_mode: u32,
    socket_mode: u32,
}

impl HostDaemonAccess {
    const fn private() -> Self {
        Self {
            runtime_dir_mode: 0o700,
            socket_mode: 0o600,
        }
    }

    pub(crate) fn for_remote_user(remote_user_id: u32, remote_group_id: u32) -> Self {
        Self::from_ids(
            current_uid(),
            current_gid(),
            remote_user_id,
            remote_group_id,
        )
    }

    const fn from_ids(
        host_user_id: u32,
        host_group_id: u32,
        remote_user_id: u32,
        remote_group_id: u32,
    ) -> Self {
        if remote_user_id == host_user_id {
            Self::private()
        } else if remote_group_id == host_group_id {
            Self {
                runtime_dir_mode: 0o710,
                socket_mode: 0o660,
            }
        } else {
            Self {
                runtime_dir_mode: 0o711,
                socket_mode: 0o666,
            }
        }
    }

    const fn expanded_for(self, required: Self) -> Self {
        Self {
            runtime_dir_mode: self.runtime_dir_mode | required.runtime_dir_mode,
            socket_mode: self.socket_mode | required.socket_mode,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HostDaemon {
    socket_path: PathBuf,
    metadata_path: PathBuf,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub(crate) enum HostDaemonStartError {
    SocketAlreadyInUse { socket_path: PathBuf },
}

impl HostDaemonStartError {
    pub(crate) fn is_socket_already_in_use(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<Self>()
            .is_some_and(|error| matches!(error, Self::SocketAlreadyInUse { .. }))
    }
}

impl fmt::Display for HostDaemonStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketAlreadyInUse { socket_path } => {
                write!(
                    formatter,
                    "Host daemon socket is already in use: {}",
                    socket_path.display()
                )
            }
        }
    }
}

impl Error for HostDaemonStartError {}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct HostDaemonMetadata {
    protocol_version: u16,
    allowed_peer_uid: u32,
    remote_gid: u32,
    git_https_mode: HostDaemonGitHttpsMode,
    container_cli: HostDaemonCliQueryIdentity,
    runtime_dir_mode: u32,
    socket_mode: u32,
    socket_dev: u64,
    socket_ino: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum HostDaemonGitHttpsMode {
    Off,
    HostHelper,
    HostHelperReadOnly,
}

impl From<GitHttpsMode> for HostDaemonGitHttpsMode {
    fn from(value: GitHttpsMode) -> Self {
        match value {
            GitHttpsMode::Off => Self::Off,
            GitHttpsMode::HostHelper => Self::HostHelper,
            GitHttpsMode::HostHelperReadOnly => Self::HostHelperReadOnly,
        }
    }
}

impl HostDaemonMetadata {
    const fn access(&self) -> HostDaemonAccess {
        HostDaemonAccess {
            runtime_dir_mode: self.runtime_dir_mode,
            socket_mode: self.socket_mode,
        }
    }
}

impl HostDaemon {
    #[cfg(test)]
    pub(crate) async fn start(runtime_dir: impl AsRef<Path>) -> Result<Self> {
        Self::start_with_git_credential_executor(runtime_dir, Arc::new(SystemGitCredentialExecutor))
            .await
    }

    #[cfg(test)]
    pub(crate) async fn start_for_remote_user(
        runtime_dir: impl AsRef<Path>,
        remote_user_id: u32,
        remote_group_id: u32,
    ) -> Result<Self> {
        Self::start_with_access(
            runtime_dir,
            HostDaemonAccess::for_remote_user(remote_user_id, remote_group_id),
            remote_user_id,
            remote_group_id,
            Arc::new(SystemGitCredentialExecutor),
            GitHttpsMode::HostHelper,
            HostDaemonCliQueryPolicy::Disabled,
        )
        .await
    }

    pub(crate) async fn start_for_remote_user_with_git_https_mode(
        runtime_dir: impl AsRef<Path>,
        remote_user_id: u32,
        remote_group_id: u32,
        git_https_mode: GitHttpsMode,
        cli_query_policy: HostDaemonCliQueryPolicy,
    ) -> Result<Self> {
        Self::start_with_access(
            runtime_dir,
            HostDaemonAccess::for_remote_user(remote_user_id, remote_group_id),
            remote_user_id,
            remote_group_id,
            Arc::new(SystemGitCredentialExecutor),
            git_https_mode,
            cli_query_policy,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_git_credential_executor(
        runtime_dir: impl AsRef<Path>,
        git_credentials: Arc<dyn GitCredentialExecutor>,
    ) -> Result<Self> {
        Self::start_with_access(
            runtime_dir,
            HostDaemonAccess::private(),
            current_uid(),
            current_gid(),
            git_credentials,
            GitHttpsMode::HostHelper,
            HostDaemonCliQueryPolicy::Disabled,
        )
        .await
    }

    async fn start_with_access(
        runtime_dir: impl AsRef<Path>,
        access: HostDaemonAccess,
        allowed_peer_uid: u32,
        remote_group_id: u32,
        git_credentials: Arc<dyn GitCredentialExecutor>,
        git_https_mode: GitHttpsMode,
        cli_query_policy: HostDaemonCliQueryPolicy,
    ) -> Result<Self> {
        let runtime_dir = runtime_dir.as_ref().to_path_buf();
        prepare_runtime_dir(&runtime_dir, access)?;
        let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);

        let listener = bind_host_daemon_socket(&socket_path).await?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(access.socket_mode))
            .with_context(|| {
                format!(
                    "Failed to set host daemon socket permissions: {}",
                    socket_path.display()
                )
            })?;
        let metadata_path = runtime_dir.join(HOST_DAEMON_METADATA_NAME);
        write_host_daemon_metadata(
            &metadata_path,
            &socket_path,
            allowed_peer_uid,
            remote_group_id,
            git_https_mode,
            &cli_query_policy.identity(),
            access,
        )
        .inspect_err(|_| {
            cleanup_host_daemon_metadata_file(&metadata_path);
            cleanup_host_daemon_socket_file(&socket_path);
        })?;

        let task = tokio::spawn(run_host_daemon(
            listener,
            allowed_peer_uid,
            git_credentials,
            git_https_mode,
            cli_query_policy,
        ));

        Ok(Self {
            socket_path,
            metadata_path,
            task: Some(task),
        })
    }

    #[cfg(test)]
    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(test)]
    pub(crate) async fn stop(mut self) -> Result<()> {
        self.shutdown().await
    }

    #[cfg(test)]
    async fn shutdown(&mut self) -> Result<()> {
        if let Some(task) = self.task.take() {
            task.abort();
            _ = task.await;
        }
        remove_metadata_if_present(&self.metadata_path)?;
        remove_socket_if_present(&self.socket_path)
    }
}

impl Drop for HostDaemon {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        cleanup_host_daemon_metadata_file(&self.metadata_path);
        cleanup_host_daemon_socket_file(&self.socket_path);
    }
}

fn prepare_runtime_dir(runtime_dir: &Path, access: HostDaemonAccess) -> Result<()> {
    create_runtime_dir(runtime_dir, "host daemon")?;
    set_private_runtime_parent(runtime_dir)?;
    set_runtime_dir_mode(runtime_dir, access.runtime_dir_mode, "host daemon")?;
    validate_runtime_dir_mode(runtime_dir, access.runtime_dir_mode, "host daemon")
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    // SAFETY: getgid has no preconditions, takes no pointers, and cannot fail.
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

async fn bind_host_daemon_socket(socket_path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            remove_stale_socket(socket_path).await?;
            UnixListener::bind(socket_path).with_context(|| {
                format!(
                    "Failed to bind host daemon socket after removing stale socket: {}",
                    socket_path.display()
                )
            })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to bind host daemon socket: {}",
                socket_path.display()
            )
        }),
    }
}

fn write_host_daemon_metadata(
    metadata_path: &Path,
    socket_path: &Path,
    allowed_peer_uid: u32,
    remote_group_id: u32,
    git_https_mode: GitHttpsMode,
    cli_query_identity: &HostDaemonCliQueryIdentity,
    access: HostDaemonAccess,
) -> Result<()> {
    let socket_metadata = fs::symlink_metadata(socket_path).with_context(|| {
        format!(
            "Failed to inspect host daemon socket metadata: {}",
            socket_path.display()
        )
    })?;
    let metadata = HostDaemonMetadata {
        protocol_version: crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION,
        allowed_peer_uid,
        remote_gid: remote_group_id,
        git_https_mode: git_https_mode.into(),
        container_cli: cli_query_identity.clone(),
        runtime_dir_mode: access.runtime_dir_mode,
        socket_mode: access.socket_mode,
        socket_dev: socket_metadata.dev(),
        socket_ino: socket_metadata.ino(),
    };
    let content =
        serde_json::to_vec(&metadata).context("Failed to serialize host daemon metadata")?;
    fs::write(metadata_path, content).with_context(|| {
        format!(
            "Failed to write host daemon metadata: {}",
            metadata_path.display()
        )
    })?;
    fs::set_permissions(metadata_path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "Failed to set host daemon metadata permissions: {}",
            metadata_path.display()
        )
    })
}

pub(crate) fn ensure_host_daemon_access_for_remote_user(
    runtime_dir: &Path,
    remote_user_id: u32,
    remote_group_id: u32,
    git_https_mode: GitHttpsMode,
    cli_query_policy: &HostDaemonCliQueryPolicy,
) -> Result<bool> {
    let metadata_path = runtime_dir.join(HOST_DAEMON_METADATA_NAME);
    let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);
    let Ok(socket_metadata) = fs::symlink_metadata(&socket_path) else {
        return Ok(false);
    };
    if !socket_metadata.file_type().is_socket() {
        return Ok(false);
    }
    let Ok(content) = fs::read(&metadata_path) else {
        return Ok(false);
    };
    let Ok(metadata) = serde_json::from_slice::<HostDaemonMetadata>(&content) else {
        return Ok(false);
    };

    if metadata.protocol_version != crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION
        || metadata.allowed_peer_uid != remote_user_id
        || metadata.git_https_mode != git_https_mode.into()
        || metadata.socket_dev != socket_metadata.dev()
        || metadata.socket_ino != socket_metadata.ino()
    {
        return Ok(false);
    }
    let cli_query_identity = cli_query_policy.identity();
    if !metadata.container_cli.can_reuse(&cli_query_identity) {
        bail!(HOST_DAEMON_QUERY_IDENTITY_MISMATCH);
    }

    let existing_access = metadata.access();
    let access = existing_access.expanded_for(HostDaemonAccess::for_remote_user(
        remote_user_id,
        remote_group_id,
    ));
    set_runtime_dir_mode(runtime_dir, access.runtime_dir_mode, "host daemon")?;
    validate_runtime_dir_mode(runtime_dir, access.runtime_dir_mode, "host daemon")?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(access.socket_mode))
        .with_context(|| {
            format!(
                "Failed to set host daemon socket permissions: {}",
                socket_path.display()
            )
        })?;
    if metadata.remote_gid != remote_group_id || existing_access != access {
        write_host_daemon_metadata(
            &metadata_path,
            &socket_path,
            remote_user_id,
            remote_group_id,
            git_https_mode,
            &cli_query_identity,
            access,
        )?;
    }

    Ok(true)
}

/// Call only after daemon startup failed because a live daemon holds the socket. A missing or
/// unreadable metadata file is not treated as incompatible, so stale-state recovery paths that
/// rely on a silent reuse decline keep restarting the daemon.
pub(crate) fn host_daemon_metadata_is_version_incompatible(runtime_dir: &Path) -> bool {
    let Ok(content) = fs::read(runtime_dir.join(HOST_DAEMON_METADATA_NAME)) else {
        return false;
    };
    match serde_json::from_slice::<HostDaemonMetadata>(&content) {
        Ok(metadata) => {
            metadata.protocol_version != crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION
        }
        Err(_) => true,
    }
}

pub(crate) async fn ensure_host_daemon_available_for_remote_user(
    runtime_dir: &Path,
    remote_user_id: u32,
    remote_group_id: u32,
    git_https_mode: GitHttpsMode,
    cli_query_policy: &HostDaemonCliQueryPolicy,
) -> Result<bool> {
    if !ensure_host_daemon_access_for_remote_user(
        runtime_dir,
        remote_user_id,
        remote_group_id,
        git_https_mode,
        cli_query_policy,
    )? {
        return Ok(false);
    }

    let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);
    match UnixStream::connect(&socket_path).await {
        Ok(_stream) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to probe host daemon socket: {}",
                socket_path.display()
            )
        }),
    }
}

async fn remove_stale_socket(socket_path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect host daemon socket path: {}",
                    socket_path.display()
                )
            });
        }
    };

    if !metadata.file_type().is_socket() {
        bail!(
            "Host daemon socket path exists but is not a socket: {}",
            socket_path.display()
        );
    }

    match UnixStream::connect(socket_path).await {
        Ok(_stream) => Err(HostDaemonStartError::SocketAlreadyInUse {
            socket_path: socket_path.to_path_buf(),
        }
        .into()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            remove_socket_file(socket_path).with_context(|| {
                format!(
                    "Failed to remove stale host daemon socket: {}",
                    socket_path.display()
                )
            })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to probe host daemon socket: {}",
                socket_path.display()
            )
        }),
    }
}

#[cfg(test)]
fn remove_socket_if_present(socket_path: &Path) -> Result<()> {
    remove_socket_file(socket_path).with_context(|| {
        format!(
            "Failed to remove host daemon socket: {}",
            socket_path.display()
        )
    })
}

#[cfg(test)]
fn remove_metadata_if_present(metadata_path: &Path) -> Result<()> {
    remove_metadata_file(metadata_path).with_context(|| {
        format!(
            "Failed to remove host daemon metadata: {}",
            metadata_path.display()
        )
    })
}

pub(crate) async fn cleanup_host_daemon_socket(runtime_dir: &Path) {
    let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);
    match remove_stale_socket(&socket_path).await {
        Ok(()) => cleanup_host_daemon_metadata_file(&runtime_dir.join(HOST_DAEMON_METADATA_NAME)),
        Err(error) => {
            crate::ui::warn(&format!(
                "Failed to remove stale host daemon socket: {}. Remove it manually if no decune process is running: {error:#}",
                socket_path.display()
            ));
        }
    }
}

fn cleanup_host_daemon_socket_file(socket_path: &Path) {
    match remove_socket_file(socket_path) {
        Ok(()) => {}
        Err(error) => crate::ui::warn(&format!(
            "Failed to remove host daemon socket: {}. Remove it manually if no decune process is running: {error}",
            socket_path.display()
        )),
    }
}

fn cleanup_host_daemon_metadata_file(metadata_path: &Path) {
    match remove_metadata_file(metadata_path) {
        Ok(()) => {}
        Err(error) => crate::ui::warn(&format!(
            "Failed to remove host daemon metadata: {}. Remove it manually if no decune process is running: {error}",
            metadata_path.display()
        )),
    }
}

fn remove_socket_file(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_metadata_file(metadata_path: &Path) -> io::Result<()> {
    match fs::remove_file(metadata_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn run_host_daemon(
    listener: UnixListener,
    allowed_peer_uid: u32,
    git_credentials: Arc<dyn GitCredentialExecutor>,
    git_https_mode: GitHttpsMode,
    cli_query_policy: HostDaemonCliQueryPolicy,
) {
    let cli_query_service = match &cli_query_policy {
        HostDaemonCliQueryPolicy::Disabled => None,
        HostDaemonCliQueryPolicy::Enabled(context) => {
            Some(Arc::new(ContainerCliQueryService::new(context.clone())))
        }
    };
    let cli_query_policy = Arc::new(cli_query_policy);
    let active_connections = Arc::new(Semaphore::new(ACTIVE_HOST_DAEMON_CONNECTIONS));
    // Keep accepted connections owned by the accept loop so cancelling the daemon
    // also cancels every in-flight connection.
    let mut connection_tasks = JoinSet::new();
    loop {
        while connection_tasks.try_join_next().is_some() {}
        let Ok(connection_permit) = Arc::clone(&active_connections).acquire_owned().await else {
            break;
        };
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        if !peer_uid_is_allowed(&stream, allowed_peer_uid) {
            continue;
        }
        drop(connection_tasks.spawn(handle_connection(
            stream,
            Arc::clone(&git_credentials),
            git_https_mode,
            Arc::clone(&cli_query_policy),
            cli_query_service.clone(),
            connection_permit,
        )));
    }
}

fn peer_uid_is_allowed(stream: &UnixStream, allowed_uid: u32) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|credentials| credentials.uid() == allowed_uid)
}

async fn handle_connection(
    mut stream: UnixStream,
    git_credentials: Arc<dyn GitCredentialExecutor>,
    git_https_mode: GitHttpsMode,
    cli_query_policy: Arc<HostDaemonCliQueryPolicy>,
    cli_query_service: Option<Arc<ContainerCliQueryService>>,
    _connection_permit: OwnedSemaphorePermit,
) {
    let Ok(request) = read_host_daemon_request(&mut stream).await else {
        return;
    };

    let response = if request.len() > MAX_HOST_DAEMON_REQUEST_BYTES {
        serialize_host_daemon_response(&HostDaemonResponse::error(
            ERROR_CODE_REQUEST_TOO_LARGE,
            format!("Host daemon request exceeds {MAX_HOST_DAEMON_REQUEST_BYTES} bytes"),
        ))
    } else {
        match handle_host_daemon_request(
            &request,
            git_credentials.as_ref(),
            git_https_mode,
            cli_query_policy.as_ref(),
        ) {
            HostDaemonRequestDispatch::Respond(response) => {
                serialize_host_daemon_response(&response)
            }
            HostDaemonRequestDispatch::CliQuery(query) => match cli_query_service {
                Some(service) => service.execute(query).await,
                None => serialize_host_daemon_response(&HostDaemonResponse::error(
                    ERROR_CODE_CLI_QUERY_FAILED,
                    "Container CLI query failed",
                )),
            },
        }
    };
    let Ok(response) = response else {
        return;
    };

    _ = write_host_daemon_response(&mut stream, &response).await;
}

fn serialize_host_daemon_response(response: &HostDaemonResponse) -> Result<Vec<u8>> {
    serde_json::to_vec(response).context("Failed to serialize host daemon response")
}

async fn read_host_daemon_request<R>(stream: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let limit = u64::try_from(MAX_HOST_DAEMON_REQUEST_BYTES + 1)
        .map_err(|error| io::Error::other(format!("Invalid host daemon request limit: {error}")))?;
    let mut request = Vec::new();
    let read = async {
        let mut limited_stream = stream.take(limit);
        limited_stream.read_to_end(&mut request).await?;
        Ok(request)
    };
    tokio::time::timeout(HOST_DAEMON_REQUEST_READ_TIMEOUT, read)
        .await
        .map_err(|_elapsed| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out reading host daemon request",
            )
        })?
}

async fn write_host_daemon_response<W>(stream: &mut W, response: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let write = async {
        stream.write_all(response).await?;
        stream.shutdown().await
    };
    tokio::time::timeout(HOST_DAEMON_RESPONSE_WRITE_TIMEOUT, write)
        .await
        .map_err(|_elapsed| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out writing host daemon response",
            )
        })?
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{Arc, Mutex},
    };

    use anyhow::{Result, anyhow, bail};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
    };

    use super::{
        ACTIVE_HOST_DAEMON_CONNECTIONS, HOST_DAEMON_REQUEST_READ_TIMEOUT,
        HOST_DAEMON_RESPONSE_WRITE_TIMEOUT, HostDaemon, HostDaemonAccess, HostDaemonGitHttpsMode,
        HostDaemonMetadata, MAX_HOST_DAEMON_REQUEST_BYTES, cleanup_host_daemon_socket, current_gid,
        current_uid, host_daemon_metadata_is_version_incompatible, peer_uid_is_allowed,
        write_host_daemon_response,
    };
    use crate::host::query_context::{HostDaemonCliQueryIdentity, HostDaemonCliQueryPolicy};
    use crate::{
        config::types::{GitHttpsMode, PortProtocol},
        docker::ports::ResolvedForwardPort,
        host::{
            credentials::{GitCredentialCommand, GitCredentialExecutor},
            forward::{ForwardStatusSource, forward_status_dir, start_forward_status_server},
        },
    };

    #[derive(Debug)]
    struct StaticGitCredentialExecutor;

    impl GitCredentialExecutor for StaticGitCredentialExecutor {
        fn run(&self, command: GitCredentialCommand, _input: &str) -> Result<String> {
            if command != GitCredentialCommand::Fill {
                bail!("Unexpected Git credential command: {command:?}");
            }
            Ok("username=octo\npassword=SECRET\n".to_owned())
        }
    }

    #[derive(Debug, Default)]
    struct RecordingGitCredentialExecutor {
        calls: Mutex<Vec<(GitCredentialCommand, String)>>,
    }

    impl GitCredentialExecutor for RecordingGitCredentialExecutor {
        fn run(&self, command: GitCredentialCommand, input: &str) -> Result<String> {
            self.calls
                .lock()
                .map_err(|error| {
                    anyhow!("Git credential call recorder mutex was poisoned: {error}")
                })?
                .push((command, input.to_owned()));
            Ok(String::new())
        }
    }

    #[test]
    fn daemon_creates_private_runtime_dir_and_socket() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("workspace-runtime");

        runtime.block_on(async {
            let daemon = HostDaemon::start(runtime_dir.clone()).await.unwrap();
            let socket_path = daemon.socket_path().to_path_buf();

            assert_eq!(socket_path, runtime_dir.join("host-daemon.sock"));
            assert_eq!(mode(&runtime_dir), 0o700);
            assert_eq!(mode(&socket_path), 0o600);

            daemon.stop().await.unwrap();
            assert!(!socket_path.exists());
        });
    }

    #[test]
    fn daemon_metadata_keeps_only_container_cli_query_identity_digest() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("workspace-runtime");
        let state_dir = temp.path().join("SECRET-state");
        let policy = HostDaemonCliQueryPolicy::enabled_for_test(
            "012345abcdef",
            state_dir.clone(),
            runtime_dir.clone(),
        );

        runtime.block_on(async {
            let daemon = HostDaemon::start_for_remote_user_with_git_https_mode(
                &runtime_dir,
                current_uid(),
                current_gid(),
                GitHttpsMode::HostHelper,
                policy,
            )
            .await
            .unwrap();

            let metadata = fs::read_to_string(runtime_dir.join("host-daemon.json")).unwrap();

            assert!(metadata.contains(r#""policy":"enabled""#));
            assert!(metadata.contains(r#""context_fingerprint":"#));
            assert!(!metadata.contains("012345abcdef"));
            assert!(!metadata.contains("SECRET-state"));
            assert!(!metadata.contains(&state_dir.display().to_string()));
            assert!(!metadata.contains(&runtime_dir.display().to_string()));

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn metadata_version_incompatibility_detects_unreadable_or_mismatched_metadata() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path();
        let metadata_path = runtime_dir.join("host-daemon.json");

        assert!(!host_daemon_metadata_is_version_incompatible(runtime_dir));

        let metadata = HostDaemonMetadata {
            protocol_version: crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION,
            allowed_peer_uid: 1000,
            remote_gid: 1000,
            git_https_mode: HostDaemonGitHttpsMode::HostHelper,
            container_cli: HostDaemonCliQueryIdentity::Disabled,
            runtime_dir_mode: 0o711,
            socket_mode: 0o666,
            socket_dev: 0,
            socket_ino: 0,
        };
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(!host_daemon_metadata_is_version_incompatible(runtime_dir));

        let mut future_version: Value = serde_json::to_value(&metadata).unwrap();
        future_version["protocol_version"] =
            json!(crate::host::protocol::HOST_DAEMON_PROTOCOL_VERSION + 1);
        fs::write(&metadata_path, serde_json::to_vec(&future_version).unwrap()).unwrap();
        assert!(host_daemon_metadata_is_version_incompatible(runtime_dir));

        // Simulates metadata written before the container_cli field existed.
        let mut old_version: Value = serde_json::to_value(&metadata).unwrap();
        old_version.as_object_mut().unwrap().remove("container_cli");
        fs::write(&metadata_path, serde_json::to_vec(&old_version).unwrap()).unwrap();
        assert!(host_daemon_metadata_is_version_incompatible(runtime_dir));

        fs::write(&metadata_path, b"not-json").unwrap();
        assert!(host_daemon_metadata_is_version_incompatible(runtime_dir));
    }

    #[test]
    fn access_policy_keeps_private_modes_when_remote_uid_matches_host_uid() {
        let access = HostDaemonAccess::from_ids(1000, 1000, 1000, 2000);

        assert_eq!(access.runtime_dir_mode, 0o700);
        assert_eq!(access.socket_mode, 0o600);
    }

    #[test]
    fn access_policy_uses_group_modes_when_only_gid_matches() {
        let access = HostDaemonAccess::from_ids(1000, 1000, 2000, 1000);

        assert_eq!(access.runtime_dir_mode, 0o710);
        assert_eq!(access.socket_mode, 0o660);
    }

    #[test]
    fn access_policy_uses_traversable_modes_when_remote_uid_and_gid_differ() {
        let access = HostDaemonAccess::from_ids(1000, 1000, 2000, 2000);

        assert_eq!(access.runtime_dir_mode, 0o711);
        assert_eq!(access.socket_mode, 0o666);
    }

    #[test]
    fn daemon_allows_remote_uid_mismatch_to_traverse_runtime_dir_and_socket() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("workspace-runtime");
        let remote_user_id = if current_uid() == 20001 { 20002 } else { 20001 };
        let remote_group_id = if current_gid() == 20001 { 20002 } else { 20001 };

        runtime.block_on(async {
            let daemon = HostDaemon::start_for_remote_user(
                runtime_dir.clone(),
                remote_user_id,
                remote_group_id,
            )
            .await
            .unwrap();
            let socket_path = daemon.socket_path().to_path_buf();

            assert_eq!(mode(&runtime_dir), 0o711);
            assert_eq!(mode(&socket_path), 0o666);

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn peer_uid_check_only_allows_configured_uid() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (stream, _peer) = UnixStream::pair().unwrap();
            let current_uid = current_uid();
            let other_uid = current_uid.wrapping_add(1);

            assert!(peer_uid_is_allowed(&stream, current_uid));
            assert!(!peer_uid_is_allowed(&stream, other_uid));
        });
    }

    #[test]
    fn daemon_rejects_unknown_protocol_version_with_structured_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 999,
                    "type": "credential"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "unsupported_protocol_version",
                        "message": "Unsupported host daemon protocol version: 999"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_start_fails_without_removing_active_socket() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let runtime_dir = temp.path().join("runtime");
            let daemon = HostDaemon::start(runtime_dir.clone()).await.unwrap();
            let socket_path = daemon.socket_path().to_path_buf();

            let second_daemon = HostDaemon::start(runtime_dir).await;
            assert!(second_daemon.is_err());

            let response = send_request(
                &socket_path,
                json!({
                    "version": 1,
                    "type": "credential"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "invalid_request",
                        "message": "Invalid Git credential request JSON: missing field `action` at line 1 column 33"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_handles_git_credential_get_request() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start_with_git_credential_executor(
                temp.path().join("runtime"),
                Arc::new(StaticGitCredentialExecutor),
            )
            .await
            .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "credential",
                    "action": "get",
                    "input": "protocol=https\nhost=github.com\n\n"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": true,
                    "output": "username=octo\npassword=SECRET\n"
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_handles_git_credential_erase_request() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let executor = Arc::new(RecordingGitCredentialExecutor::default());
            let daemon = HostDaemon::start_with_git_credential_executor(
                temp.path().join("runtime"),
                Arc::<RecordingGitCredentialExecutor>::clone(&executor),
            )
            .await
            .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "credential",
                    "action": "erase",
                    "input": "protocol=https\nhost=github.com\nusername=octo\n\n"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": true,
                    "output": ""
                })
            );
            assert_eq!(
                executor.calls.lock().unwrap().as_slice(),
                [(
                    GitCredentialCommand::Reject,
                    "protocol=https\nhost=github.com\nusername=octo\n\n".to_owned()
                )]
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_blocks_cli_query_when_container_cli_is_disabled() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "cliQuery",
                    "command": "status",
                    "format": "text"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "container_cli_disabled",
                        "message": "Container CLI queries are disabled"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_executes_status_query_when_enabled() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let runtime_dir = temp.path().join("runtime");
            let policy = HostDaemonCliQueryPolicy::enabled_for_test(
                "012345abcdef",
                temp.path().join("state"),
                runtime_dir.clone(),
            );
            let daemon = HostDaemon::start_for_remote_user_with_git_https_mode(
                &runtime_dir,
                current_uid(),
                current_gid(),
                GitHttpsMode::HostHelper,
                policy,
            )
            .await
            .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "cliQuery",
                    "command": "status",
                    "format": "text"
                }),
            )
            .await;

            assert_eq!(response["version"], 1);
            assert_eq!(response["ok"], true);
            assert!(response.get("error").is_none());
            let output = response["output"].as_str().unwrap();
            assert!(output.contains("Workspace ID: 012345abcdef"));
            assert!(output.contains("Live workspace: not checked"));
            assert!(output.ends_with('\n'));
            assert!(!output.ends_with("\n\n"));
            let serialized = response.to_string();
            assert!(!serialized.contains(&temp.path().display().to_string()));

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_rejects_oversized_request_with_structured_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();
            let request = vec![b' '; MAX_HOST_DAEMON_REQUEST_BYTES + 1];

            let response = send_raw_request(daemon.socket_path(), &request).await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "request_too_large",
                        "message": "Host daemon request exceeds 65536 bytes"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_leaves_connections_beyond_thirty_two_in_listener_backlog() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();
            let mut held_connections = Vec::new();
            for _ in 0..ACTIVE_HOST_DAEMON_CONNECTIONS {
                let mut stream = UnixStream::connect(daemon.socket_path()).await.unwrap();
                stream.write_all(b"{").await.unwrap();
                held_connections.push(stream);
                tokio::task::yield_now().await;
            }

            let mut backlogged = UnixStream::connect(daemon.socket_path()).await.unwrap();
            backlogged
                .write_all(br#"{"version":1,"type":"credential"}"#)
                .await
                .unwrap();
            backlogged.shutdown().await.unwrap();
            let mut response = Vec::new();

            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    backlogged.read_to_end(&mut response),
                )
                .await
                .is_err()
            );

            drop(held_connections.remove(0));
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                backlogged.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            let response: Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"]["code"], "invalid_request");

            drop(held_connections);
            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_request_read_timeout_releases_slow_client() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            tokio::time::pause();
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();
            let mut stream = UnixStream::connect(daemon.socket_path()).await.unwrap();
            stream.write_all(b"{").await.unwrap();
            tokio::task::yield_now().await;

            tokio::time::advance(HOST_DAEMON_REQUEST_READ_TIMEOUT).await;
            tokio::task::yield_now().await;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();

            assert!(response.is_empty());
            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_stop_aborts_active_connection_tasks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();
            let mut stream = UnixStream::connect(daemon.socket_path()).await.unwrap();
            stream.write_all(b"{").await.unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            daemon.stop().await.unwrap();

            let mut response = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                stream.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(response.is_empty());
        });
    }

    #[test]
    fn daemon_drop_aborts_active_connection_tasks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();
            let mut stream = UnixStream::connect(daemon.socket_path()).await.unwrap();
            stream.write_all(b"{").await.unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            drop(daemon);

            let mut response = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                stream.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(response.is_empty());
        });
    }

    #[test]
    fn daemon_response_write_and_shutdown_share_two_second_timeout() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            tokio::time::pause();
            let (mut writer, _reader) = tokio::io::duplex(1);
            let task = tokio::spawn(async move {
                write_host_daemon_response(&mut writer, &[b'x'; 4096]).await
            });
            tokio::task::yield_now().await;

            tokio::time::advance(HOST_DAEMON_RESPONSE_WRITE_TIMEOUT).await;
            let error = task.await.unwrap().unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        });
    }

    #[test]
    fn daemon_aggregates_forwarding_sessions_owned_outside_daemon() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let runtime_dir = temp.path().join("runtime");
            let status_dir = forward_status_dir(&runtime_dir);
            let first_server = start_forward_status_server(&status_dir).await.unwrap();
            first_server.registry().record(
                &forward_port(3001, 3000, 3000),
                ForwardStatusSource::Configured,
            );
            let second_server = start_forward_status_server(&status_dir).await.unwrap();
            second_server
                .registry()
                .record(&forward_port(5433, 5432, 5432), ForwardStatusSource::Auto);
            let policy = HostDaemonCliQueryPolicy::enabled_for_test(
                "012345abcdef",
                temp.path().join("state"),
                runtime_dir.clone(),
            );
            // The forwarding registries belong to independent session servers and are not
            // injected into the daemon. The daemon discovers every session through status_dir.
            let daemon = HostDaemon::start_for_remote_user_with_git_https_mode(
                &runtime_dir,
                current_uid(),
                current_gid(),
                GitHttpsMode::HostHelper,
                policy,
            )
            .await
            .unwrap();

            let two_sessions = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "cliQuery",
                    "command": "ports",
                    "format": "text"
                }),
            )
            .await;
            let output = two_sessions["output"].as_str().unwrap();
            assert_eq!(two_sessions["ok"], true);
            assert!(output.contains("127.0.0.1:3001"));
            assert!(output.contains("127.0.0.1:5433"));

            first_server.stop().await;
            let one_session = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "cliQuery",
                    "command": "ports",
                    "format": "text"
                }),
            )
            .await;
            let output = one_session["output"].as_str().unwrap();
            assert_eq!(one_session["ok"], true);
            assert!(!output.contains("127.0.0.1:3001"));
            assert!(output.contains("127.0.0.1:5433"));

            second_server.stop().await;
            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_removes_stale_socket_before_binding() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let runtime_dir = temp.path().join("runtime");
            fs::create_dir_all(&runtime_dir).unwrap();
            let socket_path = runtime_dir.join("host-daemon.sock");
            let stale_listener = UnixListener::bind(&socket_path).unwrap();
            drop(stale_listener);
            assert!(socket_path.exists());

            let daemon = HostDaemon::start(runtime_dir).await.unwrap();
            assert_eq!(daemon.socket_path(), socket_path.as_path());
            assert_eq!(mode(&socket_path), 0o600);

            daemon.stop().await.unwrap();
            assert!(!socket_path.exists());
        });
    }

    #[test]
    fn cleanup_removes_stale_socket_file() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let runtime_dir = temp.path().join("runtime");
            fs::create_dir_all(&runtime_dir).unwrap();
            let socket_path = runtime_dir.join("host-daemon.sock");
            let stale_listener = UnixListener::bind(&socket_path).unwrap();
            drop(stale_listener);
            assert!(socket_path.exists());

            cleanup_host_daemon_socket(&runtime_dir).await;

            assert!(!socket_path.exists());
        });
    }

    #[test]
    fn daemon_rejects_unknown_request_type_with_structured_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "runHostCommand"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "unknown_request_type",
                        "message": "Unknown host daemon request type: runHostCommand"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    #[test]
    fn daemon_keeps_port_forward_request_scoped_as_unimplemented_skeleton() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();

        runtime.block_on(async {
            let daemon = HostDaemon::start(temp.path().join("runtime"))
                .await
                .unwrap();

            let response = send_request(
                daemon.socket_path(),
                json!({
                    "version": 1,
                    "type": "portForward"
                }),
            )
            .await;

            assert_eq!(
                response,
                json!({
                    "version": 1,
                    "ok": false,
                    "error": {
                        "code": "not_implemented",
                        "message": "Host daemon request is not implemented yet: portForward"
                    }
                })
            );

            daemon.stop().await.unwrap();
        });
    }

    async fn send_request(socket_path: &Path, request: Value) -> Value {
        send_raw_request(
            socket_path,
            serde_json::to_string(&request).unwrap().as_bytes(),
        )
        .await
    }

    async fn send_raw_request(socket_path: &Path, request: &[u8]) -> Value {
        let mut stream = UnixStream::connect(socket_path).await.unwrap();
        stream.write_all(request).await.unwrap();
        stream.shutdown().await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();

        serde_json::from_slice(&response).unwrap()
    }

    fn forward_port(host: u16, requested_host: u16, container: u16) -> ResolvedForwardPort {
        ResolvedForwardPort {
            service: None,
            container,
            requested_host,
            host,
            host_ip: "127.0.0.1".to_owned(),
            protocol: PortProtocol::Tcp,
            require_local: false,
            label: None,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

use std::{
    error::Error,
    fmt, fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

use crate::{
    config::types::GitHttpsMode,
    host::{
        credentials::{GitCredentialExecutor, SystemGitCredentialExecutor},
        protocol::{HostDaemonResponse, handle_host_daemon_request},
        runtime::{
            create_runtime_dir, set_private_runtime_parent, set_runtime_dir_mode,
            validate_runtime_dir_mode,
        },
    },
};

const HOST_DAEMON_SOCKET_NAME: &str = "host-daemon.sock";
const HOST_DAEMON_METADATA_NAME: &str = "host-daemon.json";
const MAX_HOST_DAEMON_REQUEST_BYTES: usize = 64 * 1024;

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
        )
        .await
    }

    pub(crate) async fn start_for_remote_user_with_git_https_mode(
        runtime_dir: impl AsRef<Path>,
        remote_user_id: u32,
        remote_group_id: u32,
        git_https_mode: GitHttpsMode,
    ) -> Result<Self> {
        Self::start_with_access(
            runtime_dir,
            HostDaemonAccess::for_remote_user(remote_user_id, remote_group_id),
            remote_user_id,
            remote_group_id,
            Arc::new(SystemGitCredentialExecutor),
            git_https_mode,
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
            access,
        )?;
    }

    Ok(true)
}

pub(crate) async fn ensure_host_daemon_available_for_remote_user(
    runtime_dir: &Path,
    remote_user_id: u32,
    remote_group_id: u32,
    git_https_mode: GitHttpsMode,
) -> Result<bool> {
    if !ensure_host_daemon_access_for_remote_user(
        runtime_dir,
        remote_user_id,
        remote_group_id,
        git_https_mode,
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
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        if !peer_uid_is_allowed(&stream, allowed_peer_uid) {
            continue;
        }
        tokio::spawn(handle_connection(
            stream,
            Arc::clone(&git_credentials),
            git_https_mode,
        ));
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
) {
    let mut request = Vec::new();
    let read_failed = {
        let Ok(limit) = u64::try_from(MAX_HOST_DAEMON_REQUEST_BYTES + 1) else {
            return;
        };
        let mut limited_stream = (&mut stream).take(limit);
        limited_stream.read_to_end(&mut request).await.is_err()
    };
    if read_failed {
        return;
    }

    let response = if request.len() > MAX_HOST_DAEMON_REQUEST_BYTES {
        HostDaemonResponse::request_too_large(MAX_HOST_DAEMON_REQUEST_BYTES)
    } else {
        handle_host_daemon_request(&request, git_credentials.as_ref(), git_https_mode)
    };
    let Ok(response) = serde_json::to_vec(&response) else {
        return;
    };

    _ = stream.write_all(&response).await;
    _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
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
        HostDaemon, HostDaemonAccess, MAX_HOST_DAEMON_REQUEST_BYTES, cleanup_host_daemon_socket,
        current_gid, current_uid, peer_uid_is_allowed,
    };
    use crate::host::credentials::{GitCredentialCommand, GitCredentialExecutor};

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

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

use crate::host::{
    credentials::{GitCredentialExecutor, SystemGitCredentialExecutor},
    protocol::{HostDaemonResponse, handle_host_daemon_request},
    runtime::{
        create_runtime_dir, set_private_runtime_parent, set_runtime_dir_mode,
        validate_runtime_dir_mode,
    },
};

const HOST_DAEMON_SOCKET_NAME: &str = "host-daemon.sock";
const MAX_HOST_DAEMON_REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostDaemonAccess {
    runtime_dir_mode: u32,
    socket_mode: u32,
}

impl HostDaemonAccess {
    fn private() -> Self {
        Self {
            runtime_dir_mode: 0o700,
            socket_mode: 0o600,
        }
    }

    pub(crate) fn for_remote_user(remote_uid: u32, remote_gid: u32) -> Self {
        Self::from_ids(current_uid(), current_gid(), remote_uid, remote_gid)
    }

    fn from_ids(host_uid: u32, host_gid: u32, remote_uid: u32, remote_gid: u32) -> Self {
        if remote_uid == host_uid {
            Self::private()
        } else if remote_gid == host_gid {
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
}

#[derive(Debug)]
pub(crate) struct HostDaemon {
    socket_path: PathBuf,
    task: Option<JoinHandle<()>>,
}

impl HostDaemon {
    #[cfg(test)]
    pub(crate) async fn start(runtime_dir: impl AsRef<Path>) -> Result<Self> {
        Self::start_with_git_credential_executor(runtime_dir, Arc::new(SystemGitCredentialExecutor))
            .await
    }

    pub(crate) async fn start_for_remote_user(
        runtime_dir: impl AsRef<Path>,
        remote_uid: u32,
        remote_gid: u32,
    ) -> Result<Self> {
        Self::start_with_access(
            runtime_dir,
            HostDaemonAccess::for_remote_user(remote_uid, remote_gid),
            remote_uid,
            Arc::new(SystemGitCredentialExecutor),
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
            git_credentials,
        )
        .await
    }

    async fn start_with_access(
        runtime_dir: impl AsRef<Path>,
        access: HostDaemonAccess,
        allowed_peer_uid: u32,
        git_credentials: Arc<dyn GitCredentialExecutor>,
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

        let task = tokio::spawn(run_host_daemon(listener, allowed_peer_uid, git_credentials));

        Ok(Self {
            socket_path,
            task: Some(task),
        })
    }

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
            let _ = task.await;
        }
        remove_socket_if_present(&self.socket_path)
    }
}

impl Drop for HostDaemon {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
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
        Ok(_stream) => bail!(
            "Host daemon socket is already in use: {}",
            socket_path.display()
        ),
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

pub(crate) async fn cleanup_host_daemon_socket(runtime_dir: &Path) {
    let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);
    if let Err(error) = remove_stale_socket(&socket_path).await {
        crate::ui::warn(&format!(
            "Failed to remove stale host daemon socket: {}. Remove it manually if no decune process is running: {error:#}",
            socket_path.display()
        ));
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

fn remove_socket_file(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn run_host_daemon(
    listener: UnixListener,
    allowed_peer_uid: u32,
    git_credentials: Arc<dyn GitCredentialExecutor>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        if !peer_uid_is_allowed(&stream, allowed_peer_uid) {
            continue;
        }
        tokio::spawn(handle_connection(stream, Arc::clone(&git_credentials)));
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
) {
    let mut request = Vec::new();
    let read_failed = {
        let mut limited_stream = (&mut stream).take(MAX_HOST_DAEMON_REQUEST_BYTES + 1);
        limited_stream.read_to_end(&mut request).await.is_err()
    };
    if read_failed {
        return;
    }

    let response = if request.len() > MAX_HOST_DAEMON_REQUEST_BYTES as usize {
        HostDaemonResponse::request_too_large(MAX_HOST_DAEMON_REQUEST_BYTES as usize)
    } else {
        handle_host_daemon_request(&request, git_credentials.as_ref())
    };
    let Ok(response) = serde_json::to_vec(&response) else {
        return;
    };

    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
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
            assert_eq!(command, GitCredentialCommand::Fill);
            Ok("username=octo\npassword=SECRET\n".to_owned())
        }
    }

    #[derive(Debug, Default)]
    struct RecordingGitCredentialExecutor {
        calls: Mutex<Vec<(GitCredentialCommand, String)>>,
    }

    impl GitCredentialExecutor for RecordingGitCredentialExecutor {
        fn run(&self, command: GitCredentialCommand, input: &str) -> Result<String> {
            self.calls.lock().unwrap().push((command, input.to_owned()));
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
        let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
        let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };

        runtime.block_on(async {
            let daemon =
                HostDaemon::start_for_remote_user(runtime_dir.clone(), remote_uid, remote_gid)
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
                executor.clone(),
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
            let request = vec![b' '; MAX_HOST_DAEMON_REQUEST_BYTES as usize + 1];

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

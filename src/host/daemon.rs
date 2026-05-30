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
    protocol::handle_host_daemon_request,
};

const HOST_DAEMON_SOCKET_NAME: &str = "host-daemon.sock";

#[derive(Debug)]
pub(crate) struct HostDaemon {
    socket_path: PathBuf,
    task: Option<JoinHandle<()>>,
}

impl HostDaemon {
    pub(crate) async fn start(runtime_dir: impl AsRef<Path>) -> Result<Self> {
        Self::start_with_git_credential_executor(runtime_dir, Arc::new(SystemGitCredentialExecutor))
            .await
    }

    pub(crate) async fn start_with_git_credential_executor(
        runtime_dir: impl AsRef<Path>,
        git_credentials: Arc<dyn GitCredentialExecutor>,
    ) -> Result<Self> {
        let runtime_dir = runtime_dir.as_ref().to_path_buf();
        prepare_runtime_dir(&runtime_dir)?;
        let socket_path = runtime_dir.join(HOST_DAEMON_SOCKET_NAME);

        let listener = bind_host_daemon_socket(&socket_path).await?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).with_context(
            || {
                format!(
                    "Failed to set host daemon socket permissions: {}",
                    socket_path.display()
                )
            },
        )?;

        let task = tokio::spawn(run_host_daemon(listener, git_credentials));

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
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn prepare_runtime_dir(runtime_dir: &Path) -> Result<()> {
    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create host daemon runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set host daemon runtime directory permissions: {}",
            runtime_dir.display()
        )
    })
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

fn remove_socket_file(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn run_host_daemon(listener: UnixListener, git_credentials: Arc<dyn GitCredentialExecutor>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(handle_connection(stream, Arc::clone(&git_credentials)));
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    git_credentials: Arc<dyn GitCredentialExecutor>,
) {
    let mut request = Vec::new();
    if stream.read_to_end(&mut request).await.is_err() {
        return;
    }

    let response = handle_host_daemon_request(&request, git_credentials.as_ref());
    let Ok(response) = serde_json::to_vec(&response) else {
        return;
    };

    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc};

    use anyhow::Result;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
    };

    use super::HostDaemon;
    use crate::host::credentials::{GitCredentialCommand, GitCredentialExecutor};

    #[derive(Debug)]
    struct StaticGitCredentialExecutor;

    impl GitCredentialExecutor for StaticGitCredentialExecutor {
        fn run(&self, command: GitCredentialCommand, _input: &str) -> Result<String> {
            assert_eq!(command, GitCredentialCommand::Fill);
            Ok("username=octo\npassword=SECRET\n".to_owned())
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
        let mut stream = UnixStream::connect(socket_path).await.unwrap();
        stream
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .unwrap();
        stream.shutdown().await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();

        serde_json::from_slice(&response).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

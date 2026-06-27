use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

use crate::{
    config::{resolved::ResolvedGitCredentials, types::GitHttpsMode},
    docker::user::resolve_remote_user,
    host::daemon::{
        HostDaemon, HostDaemonStartError, ensure_host_daemon_access_for_remote_user,
        ensure_host_daemon_available_for_remote_user,
    },
    ui,
    up::{exec_target::resolve_up_exec_target, start::StartedUpContainer},
};

const REUSED_HOST_DAEMON_MONITOR_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub(in crate::up) struct HostDaemonGuard {
    _daemon: Option<HostDaemon>,
    monitor_task: Option<JoinHandle<()>>,
}

impl HostDaemonGuard {
    const fn owned(daemon: HostDaemon) -> Self {
        Self {
            _daemon: Some(daemon),
            monitor_task: None,
        }
    }

    fn reused(
        runtime_dir: PathBuf,
        remote_uid: u32,
        remote_gid: u32,
        git_https_mode: GitHttpsMode,
    ) -> Self {
        Self {
            _daemon: None,
            monitor_task: Some(tokio::spawn(monitor_reused_host_daemon(
                runtime_dir,
                remote_uid,
                remote_gid,
                git_https_mode,
            ))),
        }
    }
}

impl Drop for HostDaemonGuard {
    fn drop(&mut self) {
        if let Some(task) = self.monitor_task.take() {
            task.abort();
        }
    }
}

pub(in crate::up) async fn start_host_daemon_for_up(
    started: &StartedUpContainer,
) -> Result<HostDaemonGuard> {
    let runtime_dir = started.workspace.paths().runtime_dir();
    let target = resolve_up_exec_target(&started.plan, &started.outcome.container_name).await?;
    let remote_user = resolve_remote_user(
        &started.client,
        &target.id,
        &started.plan.effective_users,
        &started.plan.uid_gid_sync_plan,
    )
    .await?;

    start_host_daemon_for_remote_user(
        runtime_dir,
        started.workspace.id(),
        remote_user.uid,
        remote_user.gid,
        daemon_git_https_mode(&started.plan.config.credentials.git),
    )
    .await
}

const fn daemon_git_https_mode(credentials: &ResolvedGitCredentials) -> GitHttpsMode {
    if credentials.enabled {
        credentials.https
    } else {
        GitHttpsMode::Off
    }
}

async fn start_host_daemon_for_remote_user(
    runtime_dir: &Path,
    workspace_id: &str,
    remote_uid: u32,
    remote_gid: u32,
    git_https_mode: GitHttpsMode,
) -> Result<HostDaemonGuard> {
    match HostDaemon::start_for_remote_user_with_git_https_mode(
        runtime_dir,
        remote_uid,
        remote_gid,
        git_https_mode,
    )
    .await
    {
        Ok(daemon) => Ok(HostDaemonGuard::owned(daemon)),
        Err(error) => {
            if HostDaemonStartError::is_socket_already_in_use(&error) {
                match ensure_host_daemon_access_for_remote_user(
                    runtime_dir,
                    remote_uid,
                    remote_gid,
                    git_https_mode,
                ) {
                    Ok(true) => {
                        return Ok(HostDaemonGuard::reused(
                            runtime_dir.to_path_buf(),
                            remote_uid,
                            remote_gid,
                            git_https_mode,
                        ));
                    }
                    Ok(false) => {}
                    Err(access_error) => {
                        return Err(access_error).with_context(|| {
                            format!("Failed to start host daemon for workspace: {workspace_id}")
                        });
                    }
                }
            }
            Err(error).with_context(|| {
                format!("Failed to start host daemon for workspace: {workspace_id}")
            })
        }
    }
}

async fn monitor_reused_host_daemon(
    runtime_dir: PathBuf,
    remote_uid: u32,
    remote_gid: u32,
    git_https_mode: GitHttpsMode,
) {
    let mut _daemon = None;
    let mut warned_failure = false;
    let mut interval = tokio::time::interval(REUSED_HOST_DAEMON_MONITOR_INTERVAL);

    loop {
        interval.tick().await;

        match ensure_host_daemon_available_for_remote_user(
            &runtime_dir,
            remote_uid,
            remote_gid,
            git_https_mode,
        )
        .await
        {
            Ok(true) => {
                warned_failure = false;
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                warn_once_about_host_daemon_monitor_failure(&mut warned_failure, &error);
                continue;
            }
        }

        match HostDaemon::start_for_remote_user_with_git_https_mode(
            &runtime_dir,
            remote_uid,
            remote_gid,
            git_https_mode,
        )
        .await
        {
            Ok(restarted) => {
                _daemon = Some(restarted);
                warned_failure = false;
            }
            Err(error) if HostDaemonStartError::is_socket_already_in_use(&error) => {
                match ensure_host_daemon_access_for_remote_user(
                    &runtime_dir,
                    remote_uid,
                    remote_gid,
                    git_https_mode,
                ) {
                    Ok(true) => warned_failure = false,
                    Ok(false) => {
                        warn_once_about_host_daemon_monitor_failure(&mut warned_failure, &error);
                    }
                    Err(access_error) => warn_once_about_host_daemon_monitor_failure(
                        &mut warned_failure,
                        &access_error,
                    ),
                }
            }
            Err(error) => warn_once_about_host_daemon_monitor_failure(&mut warned_failure, &error),
        }
    }
}

fn warn_once_about_host_daemon_monitor_failure(warned: &mut bool, error: &anyhow::Error) {
    if *warned {
        return;
    }
    *warned = true;
    ui::warn(&format!(
        "Failed to keep host daemon available for this session: {error:#}"
    ));
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, fs::symlink},
        path::Path,
        time::Duration,
    };

    use tempfile::TempDir;
    use tokio::net::{UnixListener, UnixStream};

    use super::{daemon_git_https_mode, start_host_daemon_for_remote_user};
    use crate::{
        config::{resolved::ResolvedGitCredentials, types::GitHttpsMode},
        host::daemon::HostDaemon,
    };

    #[test]
    fn disabled_git_credentials_force_daemon_https_mode_off() {
        let credentials = ResolvedGitCredentials {
            enabled: false,
            https: GitHttpsMode::HostHelper,
            ..Default::default()
        };

        assert_eq!(daemon_git_https_mode(&credentials), GitHttpsMode::Off);
    }

    #[test]
    fn enabled_git_credentials_preserve_daemon_https_mode() {
        let credentials = ResolvedGitCredentials {
            enabled: true,
            https: GitHttpsMode::HostHelperReadOnly,
            ..Default::default()
        };

        assert_eq!(
            daemon_git_https_mode(&credentials),
            GitHttpsMode::HostHelperReadOnly
        );
    }

    #[test]
    fn host_daemon_skips_startup_when_daemon_already_running() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
            let existing = HostDaemon::start_for_remote_user(&runtime_dir, remote_uid, remote_gid)
                .await
                .unwrap();

            let guard = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                remote_gid,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap();

            assert_eq!(mode(&runtime_dir), 0o711);

            drop(guard);
            existing.stop().await.unwrap();
        });
    }

    #[test]
    fn host_daemon_reuse_restarts_when_existing_owner_stops() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
            let existing = HostDaemon::start_for_remote_user(&runtime_dir, remote_uid, remote_gid)
                .await
                .unwrap();

            let guard = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                remote_gid,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap();

            existing.stop().await.unwrap();

            wait_for_socket(&runtime_dir.join("host-daemon.sock")).await;
            assert_eq!(mode(&runtime_dir), 0o711);
            assert_eq!(mode(&runtime_dir.join("host-daemon.sock")), 0o666);

            drop(guard);
        });
    }

    #[test]
    fn host_daemon_reuse_expands_access_when_remote_gid_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let group_access_gid = current_gid();
            let world_access_gid = if current_gid() == 20001 { 20002 } else { 20001 };
            let existing =
                HostDaemon::start_for_remote_user(&runtime_dir, remote_uid, group_access_gid)
                    .await
                    .unwrap();

            assert_eq!(mode(&runtime_dir), 0o710);
            assert_eq!(mode(&runtime_dir.join("host-daemon.sock")), 0o660);

            let guard = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                world_access_gid,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap();

            assert_eq!(mode(&runtime_dir), 0o711);
            assert_eq!(mode(&runtime_dir.join("host-daemon.sock")), 0o666);

            drop(guard);
            existing.stop().await.unwrap();
        });
    }

    #[test]
    fn host_daemon_reuse_does_not_narrow_existing_access() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let world_access_gid = if current_gid() == 20001 { 20002 } else { 20001 };
            let group_access_gid = current_gid();
            let existing =
                HostDaemon::start_for_remote_user(&runtime_dir, remote_uid, world_access_gid)
                    .await
                    .unwrap();

            assert_eq!(mode(&runtime_dir), 0o711);
            assert_eq!(mode(&runtime_dir.join("host-daemon.sock")), 0o666);

            let guard = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                group_access_gid,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap();

            assert_eq!(mode(&runtime_dir), 0o711);
            assert_eq!(mode(&runtime_dir.join("host-daemon.sock")), 0o666);

            drop(guard);
            existing.stop().await.unwrap();
        });
    }

    #[test]
    fn host_daemon_reuse_requires_matching_git_https_mode() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
            let existing = HostDaemon::start_for_remote_user(&runtime_dir, remote_uid, remote_gid)
                .await
                .unwrap();

            let error = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                remote_gid,
                GitHttpsMode::HostHelperReadOnly,
            )
            .await
            .unwrap_err();

            assert!(
                format!("{error:#}").contains("Host daemon socket is already in use"),
                "{error:#}"
            );

            existing.stop().await.unwrap();
        });
    }

    #[test]
    fn host_daemon_start_does_not_ignore_symlink_runtime_dir_with_active_socket() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let target_dir = temp.path().join("target-runtime");
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir(&target_dir).unwrap();
        symlink(&target_dir, &runtime_dir).unwrap();

        runtime.block_on(async {
            let _listener = UnixListener::bind(target_dir.join("host-daemon.sock")).unwrap();

            let error = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                20001,
                20001,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("Failed to start host daemon for workspace: workspace-test")
            );
            assert!(
                format!("{error:#}")
                    .contains("host daemon runtime directory must not be a symlink")
            );
        });
    }

    #[test]
    fn host_daemon_start_does_not_reuse_active_socket_without_decune_metadata() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir(&runtime_dir).unwrap();

        runtime.block_on(async {
            let _listener = UnixListener::bind(runtime_dir.join("host-daemon.sock")).unwrap();

            let error = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                20001,
                20001,
                GitHttpsMode::HostHelper,
            )
            .await
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("Failed to start host daemon for workspace: workspace-test")
            );
            assert!(format!("{error:#}").contains("Host daemon socket is already in use"));
        });
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    async fn wait_for_socket(socket_path: &Path) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if UnixStream::connect(socket_path).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "host daemon socket did not become connectable: {}",
                socket_path.display()
            )
        });
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    fn current_gid() -> u32 {
        unsafe { libc::getgid() }
    }
}

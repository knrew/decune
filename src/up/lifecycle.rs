use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::task::JoinHandle;

use crate::{
    config::{resolved::ResolvedGitCredentials, types::GitHttpsMode},
    devcontainer::lifecycle::{
        LifecycleRunContext, PreparedLifecycleRunContext, prepare_container_lifecycle,
        run_attach_lifecycle, run_container_start_lifecycle,
    },
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
    fn owned(daemon: HostDaemon) -> Self {
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

pub(in crate::up) fn report_up_success(started: &StartedUpContainer, elapsed: Duration) {
    let name = &started.outcome.container_name;
    let message = match started.lifecycle_path {
        crate::devcontainer::lifecycle::LifecycleRunPath::New => {
            format!("Started dev container: {name}")
        }
        crate::devcontainer::lifecycle::LifecycleRunPath::Started => {
            format!("Started existing dev container: {name}")
        }
        crate::devcontainer::lifecycle::LifecycleRunPath::Running => {
            format!("Reusing running dev container: {name}")
        }
    };

    ui::finished(&message, elapsed);
}

pub(in crate::up) async fn prepare_up_lifecycle(
    started: &StartedUpContainer,
) -> Result<PreparedLifecycleRunContext<'_>> {
    let target = resolve_up_exec_target(&started.plan, &started.outcome.container_name).await?;
    let remote_user = resolve_remote_user(
        &started.client,
        &target.id,
        &started.plan.effective_users,
        &started.plan.uid_gid_sync_plan,
    )
    .await?;

    prepare_container_lifecycle(LifecycleRunContext {
        client: &started.client,
        container: target.id,
        config: &started.plan.config,
        workspace_root: started.workspace.root(),
        workspace_basename: started.workspace.basename(),
        workspace_id: started.workspace.id(),
        workspace_folder: &started.plan.workspace_folder,
        runtime_dir: started.workspace.paths().runtime_dir(),
        remote_user,
    })
    .await
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

fn daemon_git_https_mode(credentials: &ResolvedGitCredentials) -> GitHttpsMode {
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

pub(in crate::up) async fn run_container_start_lifecycle_for_up(
    started: &StartedUpContainer,
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    let mut lifecycle_state = started.state.borrow().lifecycle;
    run_container_start_lifecycle(
        started.lifecycle_path,
        lifecycle,
        &mut lifecycle_state,
        |updated_lifecycle| {
            let mut started_state = started.state.borrow_mut();
            started_state.lifecycle = *updated_lifecycle;
            crate::state::write_state_file(started.workspace.paths().state_dir(), &started_state)
        },
    )
    .await
}

pub(in crate::up) async fn run_attach_lifecycle_for_up(
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    run_attach_lifecycle(lifecycle).await
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::{fs::PermissionsExt, fs::symlink},
        path::Path,
        time::Duration,
    };

    use tempfile::TempDir;
    use tokio::net::{UnixListener, UnixStream};

    use super::{daemon_git_https_mode, start_host_daemon_for_remote_user};
    use crate::{
        config::{ConfigLayer, resolved::ResolvedGitCredentials, types::GitHttpsMode},
        docker::{
            client::DockerClient,
            container::remove_container,
            exec::{ExecCommandSpec, exec_capture},
            image::remove_image,
        },
        host::daemon::HostDaemon,
        up::{
            UpOptions,
            plan::build_up_plan,
            run_detached_up,
            test_support::{test_workspace, write_devcontainer},
        },
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
    #[test]
    fn up_detach_stops_lifecycle_after_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "onCreateCommand": "printf on-create >/tmp/decune-lifecycle; exit 7",
                  "updateContentCommand": "printf update-content >>/tmp/decune-lifecycle"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage onCreateCommand failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;

                assert_eq!(String::from_utf8(output.stdout).unwrap(), "on-create");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_waits_for_parallel_post_start_siblings() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-parallel-lifecycle-failure");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": {
                    "a_slow": "sleep 1; printf done >/tmp/decune-parallel-lifecycle",
                    "z_fail": "exit 7"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Lifecycle stage postStartCommand.z_fail failed"));
                assert!(message.contains("exit code 7"));

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-parallel-lifecycle".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "done");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_applies_remote_env_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-remote-env");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV_SENTINEL": "from-remote-env"
                  },
                  "postStartCommand": "test \"$DECUNE_REMOTE_ENV_SENTINEL\" = from-remote-env && printf '%s' \"$DECUNE_REMOTE_ENV_SENTINEL\" >/tmp/decune-remote-env"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-remote-env");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_applies_user_env_probe_to_lifecycle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  'export DECUNE_PROBED_ENV=from-profile' \
                  'export DECUNE_ENV_PRIORITY=from-profile' \
                  >/etc/profile.d/decune-probe.sh
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_ENV_PRIORITY": "from-remote-env"
                  },
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_PROBED_ENV\" = from-profile && test \"$DECUNE_ENV_PRIORITY\" = from-remote-env && printf '%s:%s' \"$DECUNE_PROBED_ENV\" \"$DECUNE_ENV_PRIORITY\" >/tmp/decune-user-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-user-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    "from-profile:from-remote-env"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_omits_remote_probe_env_for_root_post_start_hook() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-root-hook-user-env-probe");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_REMOTE_ONLY=from-decune' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "remoteEnv": {
                    "DECUNE_REMOTE_ENV": "from-remote-env"
                  }
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1

    [[hooks.before_post_start]]
    command = "test -z \"${DECUNE_REMOTE_ONLY+x}\" && test \"$DECUNE_REMOTE_ENV\" = from-remote-env && printf '%s' root-hook-clean >/tmp/decune-root-hook-env"
    user = "root"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-root-hook-env".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "root-hook-clean");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_probes_env_with_remote_user_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-lifecycle-user-env-probe-login-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'export DECUNE_LOGIN_SHELL_ENV=from-login-shell' \
                  'exec /bin/sh "$@"' \
                  >/usr/local/bin/decune-probe-shell \
                  && chmod +x /usr/local/bin/decune-probe-shell \
                  && adduser -D -s /usr/local/bin/decune-probe-shell decune
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "remoteUser": "decune",
                  "userEnvProbe": "loginShell",
                  "postStartCommand": [
                    "/bin/sh",
                    "-c",
                    "test \"$DECUNE_LOGIN_SHELL_ENV\" = from-login-shell && printf '%s' \"$DECUNE_LOGIN_SHELL_ENV\" >/tmp/decune-login-shell-env-probe"
                  ]
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-login-shell-env-probe".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "from-login-shell");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
}

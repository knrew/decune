use anyhow::{Context, Result};

use crate::{
    devcontainer::lifecycle::{
        LifecycleRunContext, PreparedLifecycleRunContext, prepare_container_lifecycle,
        run_attach_lifecycle, run_container_start_lifecycle,
    },
    docker::user::resolve_remote_user,
    host::daemon::HostDaemon,
    ui,
    up::{exec_target::resolve_up_exec_target, start::StartedUpContainer},
};

pub(in crate::up) fn report_up_success(started: &StartedUpContainer) {
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

    ui::done(&message);
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
) -> Result<HostDaemon> {
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
    )
    .await
}

async fn start_host_daemon_for_remote_user(
    runtime_dir: &std::path::Path,
    workspace_id: &str,
    remote_uid: u32,
    remote_gid: u32,
) -> Result<HostDaemon> {
    let daemon = HostDaemon::start_for_remote_user(runtime_dir, remote_uid, remote_gid)
        .await
        .with_context(|| format!("Failed to start host daemon for workspace: {workspace_id}"))?;
    let _socket_path = daemon.socket_path();

    Ok(daemon)
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
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use tempfile::TempDir;

    use super::start_host_daemon_for_remote_user;
    use crate::host::daemon::HostDaemon;

    #[test]
    fn host_daemon_start_does_not_reuse_active_socket() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");

        runtime.block_on(async {
            let existing = HostDaemon::start(&runtime_dir).await.unwrap();
            let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
            let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };

            let result = start_host_daemon_for_remote_user(
                &runtime_dir,
                "workspace-test",
                remote_uid,
                remote_gid,
            )
            .await;

            let message = format!("{:#}", result.unwrap_err());
            assert!(message.contains("Failed to start host daemon for workspace: workspace-test"));
            assert!(message.contains("Host daemon socket is already in use"));
            assert_eq!(mode(&runtime_dir), 0o711);

            existing.stop().await.unwrap();
        });
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    fn current_gid() -> u32 {
        unsafe { libc::getgid() }
    }
}

use anyhow::{Context, Result};

use crate::{
    devcontainer::lifecycle::{
        LifecycleRunContext, PreparedLifecycleRunContext, prepare_container_lifecycle,
        run_attach_lifecycle, run_container_start_lifecycle,
    },
    docker::user::resolve_remote_user,
    host::daemon::HostDaemon,
    ui,
    up::start::StartedUpContainer,
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
    let remote_user = resolve_remote_user(
        &started.client,
        &started.outcome.container_name,
        &started.plan.effective_users,
        &started.plan.uid_gid_sync_plan,
    )
    .await?;

    prepare_container_lifecycle(LifecycleRunContext {
        client: &started.client,
        container: &started.outcome.container_name,
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
    let remote_user = resolve_remote_user(
        &started.client,
        &started.outcome.container_name,
        &started.plan.effective_users,
        &started.plan.uid_gid_sync_plan,
    )
    .await?;

    let daemon = HostDaemon::start_for_remote_user(
        started.workspace.paths().runtime_dir(),
        remote_user.uid,
        remote_user.gid,
    )
    .await
    .with_context(|| {
        format!(
            "Failed to start host daemon for workspace: {}",
            started.workspace.id()
        )
    })?;
    let _socket_path = daemon.socket_path();

    Ok(daemon)
}

pub(in crate::up) async fn run_container_start_lifecycle_for_up(
    started: &StartedUpContainer,
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    run_container_start_lifecycle(started.lifecycle_path, lifecycle).await
}

pub(in crate::up) async fn run_attach_lifecycle_for_up(
    lifecycle: &PreparedLifecycleRunContext<'_>,
) -> Result<()> {
    run_attach_lifecycle(lifecycle).await
}

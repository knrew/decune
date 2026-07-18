use anyhow::Result;

use crate::{
    docker::client::DockerClient,
    ports::{collect_all_ports, collect_workspace_ports, sort_ports},
    state::load_state_file,
    ui,
    workspace::{Workspace, decune_state_root},
};

mod aggregate;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the container query daemon connects this pure model and renderer in follow-up task #431"
    )
)]
pub(crate) mod container;
mod evidence;
mod inventory;
mod render;
mod types;

pub(crate) use types::StatusOptions;

use evidence::{
    CurrentWorkspaceConfig, WorkspaceEvidence, collect_docker_evidence,
    collect_workspace_docker_evidence, current_workspace_config, load_status_states,
};
use inventory::{build_status_inventory, workspace_status_with_config};
use render::{render_status_summary, render_workspace_detail};
use types::{StatusInventory, WorkspaceStatus};

pub(crate) async fn discover_status_inventory() -> Result<StatusInventory> {
    let state_entries = load_status_states(&decune_state_root()?)?;
    let client = DockerClient::connect_from_env();
    let docker_evidence = collect_docker_evidence(client.cli(), &state_entries)
        .await
        .map_err(|error| format!("Failed to read decune-managed Docker resources: {error:#}"));

    Ok(build_status_inventory(state_entries, docker_evidence))
}

pub(crate) async fn run_status(options: StatusOptions) -> Result<()> {
    if let Some(path) = options.workspace {
        let workspace = Workspace::resolve(path)?;
        let current_config = current_workspace_config(&workspace)?;
        let status = discover_workspace_status(&workspace, current_config.clone()).await?;
        let mut ports = collect_workspace_ports(&workspace, false).await?;
        for warning in &ports.warnings {
            ui::warn(warning);
        }
        sort_ports(&mut ports.ports);
        print!("{}", render_workspace_detail(&status, &ports.ports));
    } else {
        let inventory = discover_status_inventory().await?;
        for issue in &inventory.issues {
            ui::warn(&issue.message);
        }
        let mut ports = collect_all_ports().await?;
        for warning in &ports.warnings {
            ui::warn(warning);
        }
        sort_ports(&mut ports.ports);
        print!("{}", render_status_summary(&inventory, &ports));
    }

    Ok(())
}

async fn discover_workspace_status(
    workspace: &Workspace,
    current_config: CurrentWorkspaceConfig,
) -> Result<WorkspaceStatus> {
    let state = match load_state_file(workspace.paths().state_dir()) {
        Ok(Some(state)) => Some(Ok(state)),
        Ok(None) => None,
        Err(error) => Some(Err(format!("{error:#}"))),
    };
    let client = DockerClient::connect_from_env();
    let state_ref = state.as_ref().and_then(|state| state.as_ref().ok());
    let docker_evidence =
        collect_workspace_docker_evidence(client.cli(), workspace.id(), state_ref)
            .await
            .map_err(|error| format!("Failed to read decune-managed Docker resources: {error:#}"));
    let docker_unavailable = docker_evidence.is_err();
    let docker_evidence = docker_evidence.unwrap_or_default();

    let evidence = WorkspaceEvidence {
        state,
        containers: docker_evidence.containers,
        volumes: docker_evidence.volumes,
    };
    let mut status = workspace_status_with_config(
        workspace.id().to_owned(),
        &evidence,
        docker_unavailable,
        Some(&current_config),
    );
    status.workspace_path = Some(workspace.root().display().to_string());

    Ok(status)
}

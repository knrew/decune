use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use bollard::models::ContainerSummary;

use crate::{
    docker::{
        client::DockerClient,
        container::{remove_container, stop_container, workspace_container_list_options},
        image::{remove_image, workspace_image_tags},
        resource::DockerResources,
        volume::{remove_volume, workspace_volumes},
    },
    state::remove_state_runtime_dirs,
    ui,
    workspace::Workspace,
};

const DEFAULT_STOP_TIMEOUT_SECONDS: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) images: bool,
    pub(crate) force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedContainer {
    id: String,
    name: String,
}

pub(crate) async fn run_down(options: DownOptions) -> Result<()> {
    let timeout_seconds = stop_timeout_seconds(options.timeout_seconds)?;
    let workspace = Workspace::resolve(&options.workspace)?;
    let client = DockerClient::connect_from_env()?;
    let containers = list_managed_containers(&client, workspace.id()).await?;

    if containers.is_empty() {
        ui::done("No dev container found for this workspace");
        return Ok(());
    }

    for container in containers {
        stop_container(&client, &container.id, timeout_seconds).await?;
        ui::done(&format!("Stopped dev container: {}", container.name));
    }

    Ok(())
}

pub(crate) async fn run_clean(options: CleanOptions) -> Result<()> {
    if clean_requires_confirmation(options.force, io::stdin().is_terminal()) && !confirm_clean()? {
        bail!("Clean cancelled");
    }

    let workspace = Workspace::resolve(&options.workspace)?;
    let client = DockerClient::connect_from_env()?;
    let containers = list_managed_containers(&client, workspace.id()).await?;

    for container in containers {
        stop_container(&client, &container.id, DEFAULT_STOP_TIMEOUT_SECONDS).await?;
        remove_container(&client, &container.id, true, true).await?;
        ui::done(&format!("Removed dev container: {}", container.name));
    }

    for volume in workspace_volumes(&client, workspace.id()).await? {
        remove_volume(&client, &volume, true).await?;
        ui::done(&format!("Removed Docker volume: {volume}"));
    }

    if options.images {
        let image_repository = DockerResources::image_repository_for_workspace(&workspace);
        for image in workspace_image_tags(&client, &image_repository).await? {
            remove_image(&client, &image, true).await?;
            ui::done(&format!("Removed Docker image: {image}"));
        }
    }

    remove_state_runtime_dirs(
        workspace.paths().state_dir(),
        workspace.paths().runtime_dir(),
    )?;
    ui::done("Cleaned dev container resources");
    Ok(())
}

pub(crate) fn clean_requires_confirmation(force: bool, stdin_is_terminal: bool) -> bool {
    !force && stdin_is_terminal
}

fn confirm_clean() -> Result<bool> {
    let mut stderr = io::stderr();
    stderr
        .write_all(b"Remove decune resources for this workspace? [y/N] ")
        .context("Failed to write clean confirmation prompt")?;
    stderr
        .flush()
        .context("Failed to flush clean confirmation prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read clean confirmation response")?;

    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn stop_timeout_seconds(timeout_seconds: u64) -> Result<i32> {
    i32::try_from(timeout_seconds).context("Stop timeout is too large")
}

async fn list_managed_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<ManagedContainer>> {
    let containers = client
        .raw()
        .list_containers(Some(workspace_container_list_options(workspace_id)))
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .into_iter()
        .filter_map(managed_container)
        .collect())
}

fn managed_container(container: ContainerSummary) -> Option<ManagedContainer> {
    let id = container.id?;
    let name = container
        .names
        .and_then(|names| names.into_iter().next())
        .map(|name| name.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| id.clone());

    Some(ManagedContainer { id, name })
}

#[cfg(test)]
mod tests {
    use super::{clean_requires_confirmation, stop_timeout_seconds};

    #[test]
    fn clean_confirmation_is_required_only_for_interactive_non_force_runs() {
        assert!(clean_requires_confirmation(false, true));
        assert!(!clean_requires_confirmation(true, true));
        assert!(!clean_requires_confirmation(false, false));
    }

    #[test]
    fn stop_timeout_rejects_values_that_docker_api_cannot_represent() {
        assert_eq!(stop_timeout_seconds(10).unwrap(), 10);
        assert!(stop_timeout_seconds(i32::MAX as u64 + 1).is_err());
    }
}

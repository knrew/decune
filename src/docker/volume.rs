use anyhow::{Context, Result};

use crate::docker::client::DockerClient;

pub(crate) async fn workspace_volumes(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<String>> {
    client
        .cli()
        .list_volumes(workspace_id)
        .await
        .with_context(|| format!("Failed to list Docker volumes for workspace: {workspace_id}"))
}

pub(crate) async fn remove_volume(client: &DockerClient, volume: &str, force: bool) -> Result<()> {
    client
        .cli()
        .remove_volume(volume, force)
        .await
        .with_context(|| format!("Failed to remove Docker volume: {volume}"))
}

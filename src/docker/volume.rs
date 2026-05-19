use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::{
    errors::Error as DockerError,
    query_parameters::{ListVolumesOptionsBuilder, RemoveVolumeOptionsBuilder},
};

use crate::docker::{client::DockerClient, resource::managed_workspace_label_filters};

pub(crate) async fn workspace_volumes(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<String>> {
    let filters = managed_workspace_label_filters(workspace_id)
        .into_iter()
        .collect::<HashMap<_, _>>();
    let options = ListVolumesOptionsBuilder::default()
        .filters(&filters)
        .build();
    let response = client
        .raw()
        .list_volumes(Some(options))
        .await
        .with_context(|| format!("Failed to list Docker volumes for workspace: {workspace_id}"))?;

    Ok(response
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect())
}

pub(crate) async fn remove_volume(client: &DockerClient, volume: &str, force: bool) -> Result<()> {
    let options = RemoveVolumeOptionsBuilder::default().force(force).build();

    match client.raw().remove_volume(volume, Some(options)).await {
        Ok(()) => Ok(()),
        Err(error) if is_volume_not_found(&error) => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove Docker volume: {volume}"))
        }
    }
}

fn is_volume_not_found(error: &DockerError) -> bool {
    matches!(
        error,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::is_volume_not_found;

    #[test]
    fn volume_not_found_is_idempotent_for_cleanup() {
        let error = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such volume".to_owned(),
        };

        assert!(is_volume_not_found(&error));
    }
}

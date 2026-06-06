use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::{ContainerCreateBody, ContainerSummary, HostConfig, VolumeCreateRequest},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
        ListImagesOptionsBuilder, ListVolumesOptionsBuilder, RemoveContainerOptionsBuilder,
        RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder, StartContainerOptionsBuilder,
    },
};
use futures_util::TryStreamExt;
use std::{collections::HashMap, path::Path};

use super::{
    Docker,
    locks::acquire_exclusive_docker_resource_lock,
    names::{workspace_id, workspace_image_repository},
};

pub(crate) async fn workspace_containers(
    workspace_root: &Path,
) -> anyhow::Result<Vec<ContainerSummary>> {
    let docker = Docker::connect_with_defaults()?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            "decune.managed=true".to_owned(),
            format!("decune.workspace={}", workspace_root.display()),
        ],
    );
    let options = ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();

    Ok(docker.list_containers(Some(options)).await?)
}

pub(crate) async fn cleanup_workspace_containers(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let containers = workspace_containers(workspace_root).await?;
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();

    for container in containers {
        if let Some(id) = container.id {
            docker.remove_container(&id, Some(options.clone())).await?;
        }
    }

    Ok(())
}

pub(crate) async fn assert_container_is_not_running(container_id: &str) {
    let docker = Docker::connect_with_defaults().unwrap();
    let inspect = docker.inspect_container(container_id, None).await.unwrap();

    assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
}

pub(crate) async fn inspect_single_workspace_container(
    workspace_root: &Path,
) -> anyhow::Result<bollard::models::ContainerInspectResponse> {
    let docker = Docker::connect_with_defaults()?;
    let containers = workspace_containers(workspace_root).await?;

    anyhow::ensure!(containers.len() == 1, "expected one workspace container");

    let id = containers[0]
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;

    Ok(docker.inspect_container(id, None).await?)
}

pub(crate) fn inspect_has_env(
    inspect: &bollard::models::ContainerInspectResponse,
    entry: &str,
) -> bool {
    inspect
        .config
        .as_ref()
        .and_then(|config| config.env.as_ref())
        .is_some_and(|env| env.iter().any(|value| value == entry))
}

pub(crate) fn inspect_has_mount_target(
    inspect: &bollard::models::ContainerInspectResponse,
    target: &str,
) -> bool {
    inspect.mounts.as_ref().is_some_and(|mounts| {
        mounts
            .iter()
            .any(|mount| mount.destination.as_deref() == Some(target))
    })
}

pub(crate) async fn exec_single_workspace_container<const N: usize>(
    workspace_root: &Path,
    command: [&str; N],
) -> anyhow::Result<String> {
    let docker = Docker::connect_with_defaults()?;
    let inspect = inspect_single_workspace_container(workspace_root).await?;
    let container_id = inspect
        .id
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;
    let options = CreateExecOptions {
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(command.into_iter().map(str::to_owned).collect::<Vec<_>>()),
        ..Default::default()
    };
    let exec = docker.create_exec(&container_id, options).await?;
    let start_options = StartExecOptions {
        detach: false,
        tty: false,
        output_capacity: None,
    };
    let StartExecResults::Attached { mut output, .. } =
        docker.start_exec(&exec.id, Some(start_options)).await?
    else {
        anyhow::bail!("Docker exec did not attach output");
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(chunk) = output.try_next().await? {
        match chunk {
            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                stdout.extend_from_slice(&message)
            }
            LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
            LogOutput::StdIn { .. } => {}
        }
    }

    let inspect = docker.inspect_exec(&exec.id).await?;
    let exit_code = inspect.exit_code.unwrap_or(-1);
    anyhow::ensure!(
        exit_code == 0,
        "Docker exec failed with exit code {exit_code}: {}",
        String::from_utf8_lossy(&stderr)
    );

    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

pub(crate) async fn workspace_volumes(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let docker = Docker::connect_with_defaults()?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            "decune.managed=true".to_owned(),
            format!("decune.workspace={}", workspace_root.display()),
        ],
    );
    let options = ListVolumesOptionsBuilder::default()
        .filters(&filters)
        .build();

    Ok(docker
        .list_volumes(Some(options))
        .await?
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect())
}

pub(crate) async fn cleanup_workspace_volumes(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let options = RemoveVolumeOptionsBuilder::default().force(true).build();

    for volume in workspace_volumes(workspace_root).await? {
        docker.remove_volume(&volume, Some(options.clone())).await?;
    }

    Ok(())
}

pub(crate) async fn workspace_images(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let docker = Docker::connect_with_defaults()?;
    let image_repository = workspace_image_repository(workspace_root);
    let mut filters = HashMap::new();
    filters.insert(
        "reference".to_owned(),
        vec![format!("{image_repository}:*")],
    );
    let options = ListImagesOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let mut images = docker
        .list_images(Some(options))
        .await?
        .into_iter()
        .flat_map(|image| image.repo_tags)
        .filter(|tag| tag.starts_with(&format!("{image_repository}:")))
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

pub(crate) async fn cleanup_workspace_images(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let options = RemoveImageOptionsBuilder::default()
        .force(true)
        .noprune(true)
        .build();
    let _lock = acquire_exclusive_docker_resource_lock()?;

    for image in workspace_images(workspace_root).await? {
        docker
            .remove_image(&image, Some(options.clone()), None)
            .await?;
    }

    Ok(())
}

pub(crate) async fn create_managed_volume(
    workspace_root: &Path,
    volume_name: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let workspace_id = workspace_id(workspace_root);
    let labels = HashMap::from([
        ("decune.managed".to_owned(), "true".to_owned()),
        (
            "decune.workspace".to_owned(),
            workspace_root.display().to_string(),
        ),
        ("decune.workspace_id".to_owned(), workspace_id),
    ]);
    let request = VolumeCreateRequest {
        name: Some(volume_name.to_owned()),
        labels: Some(labels),
        ..Default::default()
    };

    docker.create_volume(request).await?;

    Ok(())
}

pub(crate) async fn create_term_marker_container(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let workspace_id = workspace_id(workspace_root);
    let name = format!("decune-clean-term-test-{workspace_id}");
    let options = CreateContainerOptionsBuilder::default().name(&name).build();
    let labels = HashMap::from([
        ("decune.managed".to_owned(), "true".to_owned()),
        (
            "decune.workspace".to_owned(),
            workspace_root.display().to_string(),
        ),
        ("decune.workspace_id".to_owned(), workspace_id),
    ]);
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec![
            "-c".to_owned(),
            "trap 'echo term > /host/term-marker; exit 0' TERM\nwhile sleep 1 & wait $!; do :; done"
                .to_owned(),
        ]),
        labels: Some(labels),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{}:/host", workspace_root.display())]),
            ..Default::default()
        }),
        ..Default::default()
    };

    docker.create_container(Some(options), body).await?;
    docker
        .start_container(&name, Some(StartContainerOptionsBuilder::default().build()))
        .await?;

    Ok(())
}

pub(crate) async fn ensure_alpine_image(docker: &Docker) -> anyhow::Result<()> {
    if docker.inspect_image("alpine:3.20").await.is_ok() {
        return Ok(());
    }

    let options = CreateImageOptionsBuilder::default()
        .from_image("alpine")
        .tag("3.20")
        .build();
    let mut stream = docker.create_image(Some(options), None, None);

    while stream.try_next().await?.is_some() {}

    Ok(())
}

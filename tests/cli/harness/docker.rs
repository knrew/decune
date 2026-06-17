use std::{collections::HashMap, path::Path, process::Command};

use serde::Deserialize;

use super::{
    locks::acquire_exclusive_docker_resource_lock,
    names::{workspace_id, workspace_image_repository},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct Docker;

impl Docker {
    pub(crate) fn connect_with_defaults() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub(crate) async fn inspect_image(&self, image: &str) -> anyhow::Result<ImageInspect> {
        inspect_image(image).await
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ImageInspect {
    pub(crate) config: Option<ImageConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ImageConfig {
    pub(crate) labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct ContainerSummary {
    #[serde(rename = "ID")]
    pub(crate) id: Option<String>,
    #[serde(rename = "Image")]
    pub(crate) image: Option<String>,
    #[serde(rename = "State")]
    pub(crate) state: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerInspectResponse {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) config: Option<ContainerConfig>,
    pub(crate) host_config: Option<HostConfig>,
    pub(crate) mounts: Option<Vec<MountPoint>>,
    pub(crate) network_settings: Option<NetworkSettings>,
    pub(crate) state: Option<ContainerState>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerConfig {
    pub(crate) env: Option<Vec<String>>,
    pub(crate) labels: Option<HashMap<String, String>>,
    pub(crate) image: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct HostConfig {
    pub(crate) mounts: Option<Vec<MountSpec>>,
    pub(crate) extra_hosts: Option<Vec<String>>,
    pub(crate) dns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct MountSpec {
    pub(crate) source: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) read_only: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct MountPoint {
    pub(crate) destination: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerState {
    pub(crate) running: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct NetworkSettings {
    pub(crate) ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct PortBinding {
    pub(crate) host_port: Option<String>,
}

pub(crate) async fn workspace_containers(
    workspace_root: &Path,
) -> anyhow::Result<Vec<ContainerSummary>> {
    let output = docker_output([
        "ps",
        "--all",
        "--filter",
        "label=decune.managed=true",
        "--filter",
        &format!("label=decune.workspace={}", workspace_root.display()),
        "--format",
        "json",
    ])?;
    parse_json_lines(&output)
}

pub(crate) async fn cleanup_workspace_containers(workspace_root: &Path) -> anyhow::Result<()> {
    for container in workspace_containers(workspace_root).await? {
        if let Some(id) = container.id {
            let _ = docker_status(["rm", "--force", "--volumes", &id]);
        }
    }

    Ok(())
}

pub(crate) async fn assert_container_is_not_running(container_id: &str) {
    let inspect = inspect_container(container_id).await.unwrap();

    assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
}

pub(crate) async fn inspect_single_workspace_container(
    workspace_root: &Path,
) -> anyhow::Result<ContainerInspectResponse> {
    let containers = workspace_containers(workspace_root).await?;

    anyhow::ensure!(containers.len() == 1, "expected one workspace container");

    let id = containers[0]
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;

    inspect_container(id).await
}

pub(crate) async fn workspace_container_logs(workspace_root: &Path) -> anyhow::Result<String> {
    let containers = workspace_containers(workspace_root).await?;

    anyhow::ensure!(containers.len() == 1, "expected one workspace container");

    let id = containers[0]
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;
    docker_output(["logs", id])
}

pub(crate) fn inspect_has_env(inspect: &ContainerInspectResponse, entry: &str) -> bool {
    inspect
        .config
        .as_ref()
        .and_then(|config| config.env.as_ref())
        .is_some_and(|env| env.iter().any(|value| value == entry))
}

pub(crate) fn inspect_has_mount_target(inspect: &ContainerInspectResponse, target: &str) -> bool {
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
    let inspect = inspect_single_workspace_container(workspace_root).await?;
    let container_id = inspect
        .id
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;
    let mut args = vec!["exec", &container_id];
    args.extend(command);
    docker_output(args)
}

pub(crate) async fn workspace_volumes(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let output = docker_output([
        "volume",
        "ls",
        "--filter",
        "label=decune.managed=true",
        "--filter",
        &format!("label=decune.workspace={}", workspace_root.display()),
        "--format",
        "{{.Name}}",
    ])?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(crate) async fn cleanup_workspace_volumes(workspace_root: &Path) -> anyhow::Result<()> {
    for volume in workspace_volumes(workspace_root).await? {
        let _ = docker_status(["volume", "rm", "--force", &volume]);
    }

    Ok(())
}

pub(crate) async fn workspace_images(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let image_repository = workspace_image_repository(workspace_root);
    let output = docker_output([
        "image",
        "ls",
        "--all",
        "--format",
        "{{.Repository}}:{{.Tag}}",
        &format!("{image_repository}:*"),
    ])?;
    let mut images = output
        .lines()
        .map(str::trim)
        .filter(|tag| tag.starts_with(&format!("{image_repository}:")))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

pub(crate) async fn cleanup_workspace_images(workspace_root: &Path) -> anyhow::Result<()> {
    let _lock = acquire_exclusive_docker_resource_lock()?;

    for image in workspace_images(workspace_root).await? {
        let _ = docker_status(["image", "rm", "--force", "--no-prune", &image]);
    }

    Ok(())
}

pub(crate) async fn create_managed_volume(
    workspace_root: &Path,
    volume_name: &str,
) -> anyhow::Result<()> {
    let workspace_id = workspace_id(workspace_root);
    docker_status([
        "volume",
        "create",
        "--label",
        "decune.managed=true",
        "--label",
        &format!("decune.workspace={}", workspace_root.display()),
        "--label",
        &format!("decune.workspace_id={workspace_id}"),
        volume_name,
    ])?;

    Ok(())
}

pub(crate) async fn create_term_marker_container(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let workspace_id = workspace_id(workspace_root);
    let name = format!("decune-clean-term-test-{workspace_id}");
    let _ = docker_status(["rm", "--force", "--volumes", &name]);
    docker_status([
        "create",
        "--name",
        &name,
        "--label",
        "decune.managed=true",
        "--label",
        &format!("decune.workspace={}", workspace_root.display()),
        "--label",
        &format!("decune.workspace_id={workspace_id}"),
        "--mount",
        &format!("type=bind,source={},target=/host", workspace_root.display()),
        "alpine:3.20",
        "/bin/sh",
        "-c",
        "trap 'echo term > /host/term-marker; exit 0' TERM\nwhile sleep 1 & wait $!; do :; done",
    ])?;
    docker_status(["start", &name])?;

    Ok(())
}

pub(crate) async fn ensure_alpine_image(_docker: &Docker) -> anyhow::Result<()> {
    if docker_status(["image", "inspect", "alpine:3.20"]).is_ok() {
        return Ok(());
    }

    docker_status(["pull", "alpine:3.20"])?;
    Ok(())
}

pub(crate) async fn inspect_image(image: &str) -> anyhow::Result<ImageInspect> {
    let output = docker_output(["image", "inspect", image])?;
    let mut images = serde_json::from_str::<Vec<ImageInspect>>(&output)?;
    images
        .pop()
        .ok_or_else(|| anyhow::anyhow!("image inspect returned no images"))
}

async fn inspect_container(container: &str) -> anyhow::Result<ContainerInspectResponse> {
    let output = docker_output(["container", "inspect", container])?;
    let mut containers = serde_json::from_str::<Vec<ContainerInspectResponse>>(&output)?;
    containers
        .pop()
        .ok_or_else(|| anyhow::anyhow!("container inspect returned no containers"))
}

fn parse_json_lines<T: for<'de> Deserialize<'de>>(output: &str) -> anyhow::Result<Vec<T>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

pub(crate) fn docker_output<I, S>(args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("docker").args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "docker command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn docker_status<I, S>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("docker").args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "docker command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

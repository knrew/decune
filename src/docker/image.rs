use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::ui;
use crate::{
    config::ConfigLayer,
    devcontainer::metadata::parse_image_metadata_layer,
    docker::{client::DockerClient, lock::DockerResourceLock},
    host::container_tools::ContainerToolPlatform,
};

pub(crate) const DEVCONTAINER_METADATA_LABEL: &str = "devcontainer.metadata";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ImageMetadataLayers {
    pub(crate) layers: Vec<ConfigLayer>,
    pub(crate) has_forward_ports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ImageStartupCommand {
    pub(crate) entrypoint: Vec<String>,
    pub(crate) command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullPolicy {
    Missing,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalImagePresence {
    Present,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePullOutcome {
    AlreadyPresent,
    Pulled,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DockerImageInspect {
    pub(crate) id: Option<String>,
    pub(crate) config: Option<DockerImageConfig>,
    #[serde(rename = "Os")]
    pub(crate) os: Option<String>,
    pub(crate) architecture: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DockerImageConfig {
    pub(crate) labels: Option<HashMap<String, String>>,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) cmd: Option<Vec<String>>,
    pub(crate) user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub(crate) struct DockerImageSummary {
    #[serde(rename = "Repository")]
    pub(crate) repository: String,
    #[serde(rename = "Tag")]
    pub(crate) tag: String,
}

impl DockerImageSummary {
    pub(crate) fn repository_tag(self) -> Option<String> {
        if self.repository == "<none>" || self.tag == "<none>" {
            None
        } else {
            Some(format!("{}:{}", self.repository, self.tag))
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub(crate) struct DockerImagePullEvent {
    pub(crate) id: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) progress_detail: Option<DockerProgressDetail>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub(crate) struct DockerProgressDetail {
    pub(crate) current: Option<u64>,
    pub(crate) total: Option<u64>,
}

pub(crate) async fn ensure_image(
    client: &DockerClient,
    image: &str,
    policy: PullPolicy,
) -> Result<ImagePullOutcome> {
    validate_image_name(image)?;

    let presence = match policy {
        PullPolicy::Always => LocalImagePresence::Missing,
        PullPolicy::Missing => local_image_presence(client, image).await?,
    };

    if should_pull_image(policy, presence) {
        pull_image(client, image).await?;
        Ok(ImagePullOutcome::Pulled)
    } else {
        Ok(ImagePullOutcome::AlreadyPresent)
    }
}

pub(crate) async fn workspace_image_tags(
    client: &DockerClient,
    image_repository: &str,
) -> Result<Vec<String>> {
    let images = client
        .cli()
        .list_images(&format!("{image_repository}:*"))
        .await
        .with_context(|| format!("Failed to list Docker images: {image_repository}"))?;
    let repo_tags = images
        .into_iter()
        .filter_map(DockerImageSummary::repository_tag)
        .collect::<Vec<_>>();

    Ok(image_tags_for_repository(repo_tags, image_repository))
}

pub(crate) async fn remove_image(client: &DockerClient, image: &str, force: bool) -> Result<()> {
    let _lock = DockerResourceLock::acquire_exclusive_from_env()?;

    client.cli().remove_image(image, force).await
}

pub(crate) async fn tag_image(client: &DockerClient, source: &str, target: &str) -> Result<()> {
    let _lock = DockerResourceLock::acquire_shared_from_env()?;

    client.cli().tag(source, target).await
}

pub(crate) async fn image_startup_command(
    client: &DockerClient,
    image: &str,
) -> Result<ImageStartupCommand> {
    let inspect = client
        .cli()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image startup command: {image}"))?;
    let config = inspect.config.unwrap_or_default();

    Ok(ImageStartupCommand {
        entrypoint: config.entrypoint.unwrap_or_default(),
        command: config.cmd.unwrap_or_default(),
    })
}

pub(crate) async fn image_container_tool_platform(
    client: &DockerClient,
    image: &str,
) -> Result<ContainerToolPlatform> {
    let inspect = client
        .cli()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image platform: {image}"))?;
    let os = inspect
        .os
        .as_deref()
        .context("Docker image inspect did not include an OS")?;
    let arch = inspect
        .architecture
        .as_deref()
        .context("Docker image inspect did not include an architecture")?;
    ContainerToolPlatform::from_docker_os_arch(os, arch)
}

pub(crate) fn image_tags_for_repository(
    repo_tags: impl IntoIterator<Item = String>,
    image_repository: &str,
) -> Vec<String> {
    let prefix = format!("{image_repository}:");

    repo_tags
        .into_iter()
        .filter(|tag| tag.starts_with(&prefix))
        .collect()
}

pub(crate) async fn image_devcontainer_metadata_layers_with_forward_ports(
    client: &DockerClient,
    image: &str,
    include_forward_ports: bool,
) -> Result<ImageMetadataLayers> {
    let inspect = client
        .cli()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image metadata: {image}"))?;
    let labels = inspect.config.and_then(|config| config.labels);
    let label = labels
        .as_ref()
        .and_then(|labels| labels.get(DEVCONTAINER_METADATA_LABEL).map(String::as_str));

    parse_devcontainer_metadata_label_with_forward_ports(image, label, include_forward_ports)
}

pub(crate) async fn image_devcontainer_metadata_layers_if_present_with_forward_ports(
    client: &DockerClient,
    image: &str,
    include_forward_ports: bool,
) -> Result<Option<ImageMetadataLayers>> {
    let Some(inspect) = client
        .cli()
        .inspect_image_if_present(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image metadata: {image}"))?
    else {
        return Ok(None);
    };
    let labels = inspect.config.and_then(|config| config.labels);
    let label = labels
        .as_ref()
        .and_then(|labels| labels.get(DEVCONTAINER_METADATA_LABEL).map(String::as_str));

    parse_devcontainer_metadata_label_with_forward_ports(image, label, include_forward_ports)
        .map(Some)
}

#[cfg(test)]
fn parse_devcontainer_metadata_label(image: &str, label: Option<&str>) -> Result<Vec<ConfigLayer>> {
    parse_devcontainer_metadata_label_with_forward_ports(image, label, true)
        .map(|metadata| metadata.layers)
}

pub(crate) fn parse_devcontainer_metadata_label_with_forward_ports(
    image: &str,
    label: Option<&str>,
    include_forward_ports: bool,
) -> Result<ImageMetadataLayers> {
    let Some(label) = label else {
        return Ok(ImageMetadataLayers {
            layers: Vec::new(),
            has_forward_ports: false,
        });
    };

    let value: Value = serde_json::from_str(label).with_context(|| {
        format!(
            "Failed to parse Docker image label {DEVCONTAINER_METADATA_LABEL} for image: {image}"
        )
    })?;

    match value {
        Value::Object(_) => {
            let (layer, has_forward_ports) =
                metadata_value_to_layer(image, value, include_forward_ports)?;
            Ok(ImageMetadataLayers {
                layers: vec![layer],
                has_forward_ports,
            })
        }
        Value::Array(values) => {
            let entries = values
            .into_iter()
            .enumerate()
            .map(|(index, value)| match value {
                Value::Object(_) => metadata_value_to_layer(image, value, include_forward_ports),
                _ => bail!(
                    "Docker image label {DEVCONTAINER_METADATA_LABEL} for image {image} array entry {index} must be an object"
                ),
            })
            .collect::<Result<Vec<_>>>()?;
            let has_forward_ports = entries
                .iter()
                .any(|(_, has_forward_ports)| *has_forward_ports);
            Ok(ImageMetadataLayers {
                layers: entries.into_iter().map(|(layer, _)| layer).collect(),
                has_forward_ports,
            })
        }
        _ => bail!(
            "Docker image label {DEVCONTAINER_METADATA_LABEL} for image {image} must be a JSON object or array"
        ),
    }
}

#[cfg(test)]
fn has_devcontainer_metadata_label(labels: Option<&HashMap<String, String>>) -> bool {
    labels.is_some_and(|labels| labels.contains_key(DEVCONTAINER_METADATA_LABEL))
}

pub(crate) async fn local_image_presence(
    client: &DockerClient,
    image: &str,
) -> Result<LocalImagePresence> {
    match client.cli().inspect_image_if_present(image).await {
        Ok(Some(_)) => Ok(LocalImagePresence::Present),
        Ok(None) => Ok(LocalImagePresence::Missing),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to inspect Docker image: {image}"))
        }
    }
}

fn metadata_value_to_layer(
    image: &str,
    value: Value,
    include_forward_ports: bool,
) -> Result<(ConfigLayer, bool)> {
    parse_image_metadata_layer(value)
        .and_then(|metadata| {
            let has_forward_ports = !metadata.forward_ports().is_empty();
            let layer = if include_forward_ports {
                metadata.to_config_layer()?
            } else {
                metadata.to_config_layer_without_forward_ports()?
            };
            Ok((layer, has_forward_ports))
        })
        .with_context(|| {
            format!(
                "Failed to convert Docker image label {DEVCONTAINER_METADATA_LABEL} for image: {image}"
            )
        })
}

async fn pull_image(client: &DockerClient, image: &str) -> Result<()> {
    let spinner = ui::spinner(&format!("Pulling Docker image: {image}"));
    let output = client.cli().pull(image).await?;
    drop(output);
    spinner.finish(&format!("Pulled Docker image: {image}"));
    Ok(())
}

pub(crate) fn validate_image_name(image: &str) -> Result<()> {
    if image.trim().is_empty() {
        bail!("Docker image name must not be empty");
    }
    if image.trim() != image {
        bail!("Docker image name must not contain surrounding whitespace: {image:?}");
    }
    if image
        .chars()
        .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        bail!("Docker image name contains unsupported whitespace or control characters: {image:?}");
    }

    Ok(())
}

const fn should_pull_image(policy: PullPolicy, presence: LocalImagePresence) -> bool {
    matches!(policy, PullPolicy::Always)
        || matches!(
            (policy, presence),
            (PullPolicy::Missing, LocalImagePresence::Missing)
        )
}

#[cfg(test)]
fn image_reference_has_tag_or_digest(image: &str) -> bool {
    if image.contains('@') {
        return true;
    }

    let name = image.rsplit('/').next().unwrap_or(image);
    name.contains(':')
}

#[cfg(test)]
fn progress_line(event: &DockerImagePullEvent) -> Option<String> {
    let status = event.status.as_deref()?;
    let progress = event
        .progress_detail
        .as_ref()
        .and_then(progress_detail_line);

    match (event.id.as_deref(), progress) {
        (Some(id), Some(progress)) => Some(format!("{id}: {status} {progress}")),
        (Some(id), None) => Some(format!("{id}: {status}")),
        (None, Some(progress)) => Some(format!("{status} {progress}")),
        (None, None) => Some(status.to_owned()),
    }
}

#[cfg(test)]
fn progress_detail_line(progress_detail: &DockerProgressDetail) -> Option<String> {
    match (progress_detail.current, progress_detail.total) {
        (Some(current), Some(total)) => Some(format!("{current}/{total} bytes")),
        (Some(current), None) => Some(format!("{current} bytes")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        DockerImagePullEvent, DockerProgressDetail, LocalImagePresence, PullPolicy,
        has_devcontainer_metadata_label, image_reference_has_tag_or_digest,
        image_tags_for_repository, parse_devcontainer_metadata_label, progress_line,
        should_pull_image, validate_image_name,
    };

    #[test]
    fn pull_policy_always_pulls_even_when_image_exists_locally() {
        assert!(should_pull_image(
            PullPolicy::Always,
            LocalImagePresence::Present
        ));
    }

    #[test]
    fn missing_policy_skips_pull_when_image_exists_locally() {
        assert!(!should_pull_image(
            PullPolicy::Missing,
            LocalImagePresence::Present
        ));
    }

    #[test]
    fn missing_policy_pulls_when_image_is_absent_locally() {
        assert!(should_pull_image(
            PullPolicy::Missing,
            LocalImagePresence::Missing
        ));
    }

    #[test]
    fn image_name_validation_rejects_whitespace_and_control_characters() {
        for image in ["", " ", " alpine:3.20", "alpine:3.20 ", "alpine\nRUN true"] {
            let error = validate_image_name(image).unwrap_err();
            assert!(
                error.to_string().contains("Docker image name"),
                "{image:?}: {error:#}"
            );
        }

        validate_image_name("alpine:3.20").unwrap();
        validate_image_name("ghcr.io/example/tool@sha256:abc123").unwrap();
    }

    #[test]
    fn progress_line_includes_layer_status_and_byte_counts() {
        let event = DockerImagePullEvent {
            id: Some("layer-1".to_owned()),
            status: Some("Downloading".to_owned()),
            progress_detail: Some(DockerProgressDetail {
                current: Some(1024),
                total: Some(2048),
            }),
        };

        assert_eq!(
            progress_line(&event).as_deref(),
            Some("layer-1: Downloading 1024/2048 bytes")
        );
    }

    #[test]
    fn progress_line_uses_status_when_layer_id_is_absent() {
        let event = DockerImagePullEvent {
            status: Some("Pull complete".to_owned()),
            ..DockerImagePullEvent::default()
        };

        assert_eq!(progress_line(&event).as_deref(), Some("Pull complete"));
    }

    #[test]
    fn tag_or_digest_detection_matches_docker_reference_forms() {
        assert!(!image_reference_has_tag_or_digest("ubuntu"));
        assert!(image_reference_has_tag_or_digest("ubuntu:24.04"));
        assert!(!image_reference_has_tag_or_digest(
            "localhost:5000/team/image"
        ));
        assert!(image_reference_has_tag_or_digest(
            "ubuntu@sha256:0123456789abcdef"
        ));
    }

    #[test]
    fn image_tags_for_repository_filters_only_matching_repository() {
        let tags = image_tags_for_repository(
            vec![
                "decune/project:abc".to_owned(),
                "decune/project:def".to_owned(),
                "decune/other:abc".to_owned(),
            ],
            "decune/project",
        );

        assert_eq!(tags, vec!["decune/project:abc", "decune/project:def"]);
    }

    #[test]
    fn metadata_label_presence_checks_label_key() {
        assert!(has_devcontainer_metadata_label(Some(&HashMap::from([(
            "devcontainer.metadata".to_owned(),
            "{}".to_owned()
        )]))));
        assert!(!has_devcontainer_metadata_label(Some(&HashMap::new())));
        assert!(!has_devcontainer_metadata_label(None));
    }

    #[test]
    fn parses_object_metadata_label() {
        let layers = parse_devcontainer_metadata_label(
            "example",
            Some(r#"{"remoteUser":"vscode","forwardPorts":[3000]}"#),
        )
        .unwrap();

        assert_eq!(layers.len(), 1);
    }
}

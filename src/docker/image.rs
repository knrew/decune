#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use bollard::{
    errors::Error as DockerError,
    models::CreateImageInfo,
    query_parameters::{
        CreateImageOptions, CreateImageOptionsBuilder, ListImagesOptionsBuilder,
        RemoveImageOptionsBuilder,
    },
};
use futures_util::TryStreamExt;
use serde_json::Value;

use crate::ui;
use crate::{
    config::ConfigLayer, devcontainer::metadata::parse_image_metadata_layer,
    docker::client::DockerClient,
};

pub(crate) const DEVCONTAINER_METADATA_LABEL: &str = "devcontainer.metadata";

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
    let mut filters = HashMap::new();
    filters.insert(
        "reference".to_owned(),
        vec![format!("{image_repository}:*")],
    );
    let options = ListImagesOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let images = client
        .raw()
        .list_images(Some(options))
        .await
        .with_context(|| format!("Failed to list Docker images: {image_repository}"))?;
    let repo_tags = images
        .into_iter()
        .flat_map(|image| image.repo_tags)
        .collect::<Vec<_>>();

    Ok(image_tags_for_repository(repo_tags, image_repository))
}

pub(crate) async fn remove_image(client: &DockerClient, image: &str, force: bool) -> Result<()> {
    let options = RemoveImageOptionsBuilder::default()
        .force(force)
        .noprune(false)
        .build();

    match client.raw().remove_image(image, Some(options), None).await {
        Ok(_) => Ok(()),
        Err(error) if is_image_not_found(&error) => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Failed to remove Docker image: {image}")),
    }
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

pub(crate) async fn image_devcontainer_metadata_layers(
    client: &DockerClient,
    image: &str,
) -> Result<Vec<ConfigLayer>> {
    let inspect = client
        .raw()
        .inspect_image(image)
        .await
        .with_context(|| format!("Failed to inspect Docker image metadata: {image}"))?;
    let labels = inspect.config.and_then(|config| config.labels);
    let label = labels
        .as_ref()
        .and_then(|labels| labels.get(DEVCONTAINER_METADATA_LABEL).map(String::as_str));

    parse_devcontainer_metadata_label(image, label)
}

pub(crate) fn parse_devcontainer_metadata_label(
    image: &str,
    label: Option<&str>,
) -> Result<Vec<ConfigLayer>> {
    let Some(label) = label else {
        return Ok(Vec::new());
    };

    let value: Value = serde_json::from_str(label).with_context(|| {
        format!(
            "Failed to parse Docker image label {DEVCONTAINER_METADATA_LABEL} for image: {image}"
        )
    })?;

    match value {
        Value::Object(_) => Ok(vec![metadata_value_to_layer(image, value)?]),
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| match value {
                Value::Object(_) => metadata_value_to_layer(image, value),
                _ => bail!(
                    "Docker image label {DEVCONTAINER_METADATA_LABEL} for image {image} array entry {index} must be an object"
                ),
            })
            .collect(),
        _ => bail!(
            "Docker image label {DEVCONTAINER_METADATA_LABEL} for image {image} must be a JSON object or array"
        ),
    }
}

async fn local_image_presence(client: &DockerClient, image: &str) -> Result<LocalImagePresence> {
    match client.raw().inspect_image(image).await {
        Ok(_) => Ok(LocalImagePresence::Present),
        Err(error) if is_image_not_found(&error) => Ok(LocalImagePresence::Missing),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to inspect Docker image: {image}"))
        }
    }
}

fn metadata_value_to_layer(image: &str, value: Value) -> Result<ConfigLayer> {
    parse_image_metadata_layer(value)
        .and_then(|metadata| metadata.to_config_layer())
        .with_context(|| {
            format!(
                "Failed to convert Docker image label {DEVCONTAINER_METADATA_LABEL} for image: {image}"
            )
        })
}

async fn pull_image(client: &DockerClient, image: &str) -> Result<()> {
    ui::info(&format!("Pulling Docker image: {image}"));

    let options = create_image_options_for_pull(image);
    let stream = client.raw().create_image(Some(options), None, None);
    futures_util::pin_mut!(stream);

    while let Some(event) = stream
        .try_next()
        .await
        .with_context(|| format!("Failed to pull Docker image: {image}"))?
    {
        if let Some(line) = progress_line(&event) {
            ui::info(&line);
        }
    }

    ui::done(&format!("Pulled Docker image: {image}"));
    Ok(())
}

fn validate_image_name(image: &str) -> Result<()> {
    if image.trim().is_empty() {
        bail!("Docker image name must not be empty");
    }

    Ok(())
}

fn should_pull_image(policy: PullPolicy, presence: LocalImagePresence) -> bool {
    matches!(policy, PullPolicy::Always)
        || matches!(
            (policy, presence),
            (PullPolicy::Missing, LocalImagePresence::Missing)
        )
}

fn create_image_options_for_pull(image: &str) -> CreateImageOptions {
    let mut builder = CreateImageOptionsBuilder::default().from_image(image);

    if !image_reference_has_tag_or_digest(image) {
        builder = builder.tag("latest");
    }

    builder.build()
}

fn image_reference_has_tag_or_digest(image: &str) -> bool {
    if image.contains('@') {
        return true;
    }

    let name = image.rsplit('/').next().unwrap_or(image);
    name.contains(':')
}

fn is_image_not_found(error: &DockerError) -> bool {
    matches!(
        error,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn progress_line(event: &CreateImageInfo) -> Option<String> {
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

fn progress_detail_line(progress_detail: &bollard::models::ProgressDetail) -> Option<String> {
    match (progress_detail.current, progress_detail.total) {
        (Some(current), Some(total)) => Some(format!("{current}/{total} bytes")),
        (Some(current), None) => Some(format!("{current} bytes")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{CreateImageInfo, ProgressDetail};

    use crate::docker::client::DockerClient;

    use super::{
        ImagePullOutcome, LocalImagePresence, PullPolicy, create_image_options_for_pull,
        ensure_image, image_tags_for_repository, parse_devcontainer_metadata_label, progress_line,
        should_pull_image,
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
    fn progress_line_includes_layer_status_and_byte_counts() {
        let event = CreateImageInfo {
            id: Some("layer-1".to_owned()),
            status: Some("Downloading".to_owned()),
            progress_detail: Some(ProgressDetail {
                current: Some(1024),
                total: Some(2048),
            }),
            ..Default::default()
        };

        assert_eq!(
            progress_line(&event).as_deref(),
            Some("layer-1: Downloading 1024/2048 bytes")
        );
    }

    #[test]
    fn progress_line_uses_status_when_layer_id_is_absent() {
        let event = CreateImageInfo {
            status: Some("Pull complete".to_owned()),
            ..Default::default()
        };

        assert_eq!(progress_line(&event).as_deref(), Some("Pull complete"));
    }

    #[test]
    fn pull_options_default_tagless_image_to_latest() {
        let options = create_image_options_for_pull("ubuntu");

        assert_eq!(options.from_image.as_deref(), Some("ubuntu"));
        assert_eq!(options.tag.as_deref(), Some("latest"));
    }

    #[test]
    fn pull_options_preserve_explicit_tag() {
        let options = create_image_options_for_pull("ubuntu:24.04");

        assert_eq!(options.from_image.as_deref(), Some("ubuntu:24.04"));
        assert_eq!(options.tag, None);
    }

    #[test]
    fn pull_options_default_registry_image_without_tag_to_latest() {
        let options = create_image_options_for_pull("localhost:5000/team/image");

        assert_eq!(
            options.from_image.as_deref(),
            Some("localhost:5000/team/image")
        );
        assert_eq!(options.tag.as_deref(), Some("latest"));
    }

    #[test]
    fn pull_options_preserve_digest_reference() {
        let options = create_image_options_for_pull(
            "ubuntu@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );

        assert_eq!(
            options.from_image.as_deref(),
            Some("ubuntu@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(options.tag, None);
    }

    #[test]
    fn image_tags_for_repository_selects_only_decune_workspace_repository_tags() {
        let tags = image_tags_for_repository(
            vec![
                "decune/project-abc123:hash1".to_owned(),
                "decune/project-abc123:hash2".to_owned(),
                "decune/project-other:hash1".to_owned(),
                "alpine:3.20".to_owned(),
            ],
            "decune/project-abc123",
        );

        assert_eq!(
            tags,
            vec![
                "decune/project-abc123:hash1".to_owned(),
                "decune/project-abc123:hash2".to_owned(),
            ]
        );
    }

    #[test]
    fn image_metadata_label_object_is_converted_to_config_layer() {
        let layers = parse_devcontainer_metadata_label(
            "example/devcontainer:latest",
            Some(
                r#"{
                    "remoteUser": "vscode",
                    "remoteEnv": {
                        "FROM_IMAGE": "1"
                    }
                }"#,
            ),
        )
        .unwrap();

        assert_eq!(layers.len(), 1);
        let devcontainer = layers[0].devcontainer.as_ref().unwrap();
        assert_eq!(devcontainer.remote_user.as_deref(), Some("vscode"));
        assert_eq!(
            devcontainer
                .remote_env
                .get("FROM_IMAGE")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn image_metadata_label_array_preserves_layer_order() {
        let layers = parse_devcontainer_metadata_label(
            "example/devcontainer:latest",
            Some(
                r#"[
                    {
                        "remoteUser": "image-user",
                        "remoteEnv": {
                            "FIRST": "1"
                        }
                    },
                    {
                        "remoteUser": "second-user",
                        "remoteEnv": {
                            "SECOND": "2"
                        }
                    }
                ]"#,
            ),
        )
        .unwrap();

        assert_eq!(layers.len(), 2);
        assert_eq!(
            layers[0]
                .devcontainer
                .as_ref()
                .unwrap()
                .remote_user
                .as_deref(),
            Some("image-user")
        );
        assert_eq!(
            layers[1]
                .devcontainer
                .as_ref()
                .unwrap()
                .remote_user
                .as_deref(),
            Some("second-user")
        );
    }

    #[test]
    fn image_metadata_label_rejects_initialize_command() {
        let error = parse_devcontainer_metadata_label(
            "example/devcontainer:latest",
            Some(r#"{"initializeCommand": "echo image init"}"#),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("example/devcontainer:latest"));
        assert!(message.contains("devcontainer.metadata"));
        assert!(message.contains("initializeCommand"));
    }

    #[test]
    fn invalid_image_metadata_label_error_mentions_image_and_label() {
        let error = parse_devcontainer_metadata_label(
            "example/devcontainer:latest",
            Some(r#"{"remoteUser": "vscode""#),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("example/devcontainer:latest"));
        assert!(message.contains("devcontainer.metadata"));
    }

    #[test]
    fn image_metadata_label_array_entries_must_be_objects() {
        let error =
            parse_devcontainer_metadata_label("example/devcontainer:latest", Some(r#"[true]"#))
                .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("example/devcontainer:latest"));
        assert!(message.contains("devcontainer.metadata"));
        assert!(message.contains("array entry 0"));
    }

    #[test]
    fn public_image_can_be_pulled_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            let outcome = ensure_image(&client, "hello-world:latest", PullPolicy::Always)
                .await
                .unwrap();

            assert_eq!(outcome, ImagePullOutcome::Pulled);
            client
                .raw()
                .inspect_image("hello-world:latest")
                .await
                .unwrap();
        });
    }

    #[test]
    fn missing_policy_uses_local_image_when_docker_tests_are_enabled() {
        if !docker_tests_enabled() {
            eprintln!("skipped: set DECUNE_DOCKER_TESTS=1 to run Docker integration tests");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let client = DockerClient::connect_from_env().unwrap();
            ensure_image(&client, "hello-world:latest", PullPolicy::Always)
                .await
                .unwrap();

            let outcome = ensure_image(&client, "hello-world:latest", PullPolicy::Missing)
                .await
                .unwrap();

            assert_eq!(outcome, ImagePullOutcome::AlreadyPresent);
        });
    }

    fn docker_tests_enabled() -> bool {
        std::env::var_os("DECUNE_DOCKER_TESTS").is_some_and(|value| value == "1")
    }
}

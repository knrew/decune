#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use bollard::{
    errors::Error as DockerError,
    models::CreateImageInfo,
    query_parameters::{CreateImageOptions, CreateImageOptionsBuilder},
};
use futures_util::TryStreamExt;

use crate::docker::client::DockerClient;
use crate::ui;

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

async fn local_image_presence(client: &DockerClient, image: &str) -> Result<LocalImagePresence> {
    match client.raw().inspect_image(image).await {
        Ok(_) => Ok(LocalImagePresence::Present),
        Err(error) if is_image_not_found(&error) => Ok(LocalImagePresence::Missing),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to inspect Docker image: {image}"))
        }
    }
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
        ensure_image, progress_line, should_pull_image,
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

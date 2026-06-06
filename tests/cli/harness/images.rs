use bollard::{
    models::{ContainerConfig, ContainerCreateBody},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder,
        RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, StartContainerOptionsBuilder,
        TagImageOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path};

use super::{
    Docker,
    docker::ensure_alpine_image,
    locks::{acquire_exclusive_docker_resource_lock, acquire_shared_docker_resource_lock},
    names::{hex_lower, workspace_id, workspace_image_repository},
};

pub(crate) async fn create_workspace_image_tag(
    workspace_root: &Path,
    tag: &str,
) -> anyhow::Result<String> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let image_repository = workspace_image_repository(workspace_root);
    let options = TagImageOptionsBuilder::default()
        .repo(&image_repository)
        .tag(tag)
        .build();

    let _lock = acquire_shared_docker_resource_lock()?;
    docker.tag_image("alpine:3.20", Some(options)).await?;

    Ok(format!("{image_repository}:{tag}"))
}

pub(crate) async fn create_image_without_devcontainer_metadata(
    image_tag: &str,
) -> anyhow::Result<()> {
    create_image_with_cmd(image_tag, Vec::new()).await
}

pub(crate) async fn create_image_with_cmd(image_tag: &str, cmd: Vec<&str>) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    if cmd.is_empty() {
        let (repo, tag) = image_tag
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
        let options = TagImageOptionsBuilder::default()
            .repo(repo)
            .tag(tag)
            .build();

        let _lock = acquire_shared_docker_resource_lock()?;
        docker.tag_image("alpine:3.20", Some(options)).await?;
        return Ok(());
    }

    commit_alpine_configured_image(
        image_tag,
        ContainerConfig {
            cmd: Some(cmd.into_iter().map(ToOwned::to_owned).collect()),
            ..Default::default()
        },
    )
    .await
}

pub(crate) async fn create_image_with_github_cli(image_tag: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-github-cli-source-{}",
        &hex_lower(&Sha256::digest(image_tag.as_bytes()))[..12]
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let script = r#"
        set -eu
        printf '%s\n' \
          '#!/bin/sh' \
          'set -eu' \
          'if [ "$1" = auth ] && [ "$2" = login ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  mkdir -p "$GH_CONFIG_DIR"' \
          '  cat > "$GH_CONFIG_DIR/token"' \
          '  exit 0' \
          'fi' \
          'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  exit 0' \
          'fi' \
          'echo "unexpected fake gh command: $*" >&2' \
          'exit 91' \
          >/usr/local/bin/gh
        chmod +x /usr/local/bin/gh
    "#;
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
        ..Default::default()
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("GitHub CLI fixture container wait stream ended"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "GitHub CLI fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();

    {
        let _lock = acquire_shared_docker_resource_lock()?;
        docker
            .commit_container(commit_options, ContainerConfig::default())
            .await?;
    }
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

pub(crate) async fn create_image_with_devcontainer_metadata_label(
    image_tag: &str,
    metadata: &str,
) -> anyhow::Result<()> {
    create_image_with_devcontainer_metadata_label_and_cmd(image_tag, metadata, vec!["true"]).await
}

pub(crate) async fn create_image_with_devcontainer_metadata_label_and_cmd(
    image_tag: &str,
    metadata: &str,
    cmd: Vec<&str>,
) -> anyhow::Result<()> {
    let labels = HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]);
    commit_alpine_configured_image(
        image_tag,
        ContainerConfig {
            labels: Some(labels),
            cmd: Some(cmd.into_iter().map(ToOwned::to_owned).collect()),
            ..Default::default()
        },
    )
    .await
}

pub(crate) async fn create_nonroot_image_with_devcontainer_metadata_label_and_cmd(
    image_tag: &str,
    metadata: &str,
    cmd: Vec<&str>,
) -> anyhow::Result<()> {
    let labels = HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]);
    commit_alpine_image_from_script(
        image_tag,
        Some("adduser -D -u 20001 app\n"),
        ContainerConfig {
            labels: Some(labels),
            cmd: Some(cmd.into_iter().map(ToOwned::to_owned).collect()),
            user: Some("app".to_owned()),
            ..Default::default()
        },
    )
    .await
}

async fn commit_alpine_configured_image(
    image_tag: &str,
    config: ContainerConfig,
) -> anyhow::Result<()> {
    commit_alpine_image_from_script(image_tag, None, config).await
}

async fn commit_alpine_image_from_script(
    image_tag: &str,
    script: Option<&str>,
    config: ContainerConfig,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-configured-image-{}",
        image_tag
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let body = match script {
        Some(script) => ContainerCreateBody {
            image: Some("alpine:3.20".to_owned()),
            entrypoint: Some(vec!["/bin/sh".to_owned()]),
            cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
            ..Default::default()
        },
        None => ContainerCreateBody {
            image: Some("alpine:3.20".to_owned()),
            cmd: Some(vec!["true".to_owned()]),
            ..Default::default()
        },
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("container wait stream ended before status"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "image metadata label fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();

    {
        let _lock = acquire_shared_docker_resource_lock()?;
        docker.commit_container(commit_options, config).await?;
    }
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

pub(crate) async fn tag_image(source: &str, target: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let (repo, tag) = target
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {target}"))?;
    let options = TagImageOptionsBuilder::default()
        .repo(repo)
        .tag(tag)
        .build();

    let _lock = acquire_shared_docker_resource_lock()?;
    docker.tag_image(source, Some(options)).await?;

    Ok(())
}

pub(crate) async fn create_image_with_devcontainer_metadata(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-image-metadata-source-{}",
        workspace_id(workspace_root)
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let script = r#"
        set -eu
        adduser -D -u 1000 -h /home/devuser devuser
        cat >/usr/local/bin/decune-record-shell <<'EOF'
#!/bin/sh
set -eu
actual_user="$(id -un)"
expected_user="${EXPECTED_USER:-}"
if [ "$actual_user" != "$expected_user" ]; then
    echo "expected shell user $expected_user, got $actual_user" >&2
    exit 11
fi
if [ "${FROM_IMAGE:-}" != "label" ]; then
    echo "expected FROM_IMAGE=label, got ${FROM_IMAGE:-}" >&2
    exit 12
fi
exit 0
EOF
        chmod +x /usr/local/bin/decune-record-shell
    "#;
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
        ..Default::default()
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("container wait stream ended before status"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "image metadata fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let metadata = r#"{"remoteUser":"devuser","remoteEnv":{"FROM_IMAGE":"label","EXPECTED_USER":"devuser"},"postStartCommand":"actual_user=$(id -un); expected_user=${EXPECTED_USER:-}; if [ \"$actual_user\" != \"$expected_user\" ]; then echo \"expected lifecycle user $expected_user, got $actual_user\" >&2; exit 11; fi; if [ \"${FROM_IMAGE:-}\" != \"label\" ]; then echo \"expected FROM_IMAGE=label, got ${FROM_IMAGE:-}\" >&2; exit 12; fi"}"#;
    let labels = HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]);
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();
    let config = ContainerConfig {
        user: Some("root".to_owned()),
        labels: Some(labels),
        ..Default::default()
    };

    {
        let _lock = acquire_shared_docker_resource_lock()?;
        docker.commit_container(commit_options, config).await?;
    }
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

pub(crate) async fn create_image_with_nonstandard_home_user(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-remote-user-home-source-{}",
        workspace_id(workspace_root)
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let script = r#"
        set -eu
        adduser -D -h /usr/local/share/node node
    "#;
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
        ..Default::default()
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("container wait stream ended before status"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "nonstandard home fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();

    {
        let _lock = acquire_shared_docker_resource_lock()?;
        docker
            .commit_container(commit_options, ContainerConfig::default())
            .await?;
    }
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

pub(crate) async fn remove_image_if_exists(image: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let _lock = acquire_exclusive_docker_resource_lock()?;

    if docker.inspect_image(image).await.is_err() {
        return Ok(());
    }

    let options = RemoveImageOptionsBuilder::default()
        .force(true)
        .noprune(true)
        .build();
    docker.remove_image(image, Some(options), None).await?;

    Ok(())
}

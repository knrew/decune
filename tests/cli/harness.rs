use assert_cmd::Command;
pub(crate) use bollard::Docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::{
        ContainerConfig, ContainerCreateBody, ContainerSummary, HostConfig, VolumeCreateRequest,
    },
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListVolumesOptionsBuilder,
        RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
        StartContainerOptionsBuilder, TagImageOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use flate2::{Compression, write::GzEncoder};
use futures_util::TryStreamExt;
pub(crate) use predicates::prelude::*;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Write, path::Path};
pub(crate) use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
};
use tar::{Builder, Header};

pub(crate) use crate::support;

pub(crate) fn decune() -> Command {
    let gh_config_dir =
        std::env::temp_dir().join(format!("decune-cli-test-empty-gh-{}", std::process::id()));
    std::fs::create_dir_all(&gh_config_dir).unwrap();

    let mut command = Command::cargo_bin("decune").unwrap();
    command
        .env("GH_CONFIG_DIR", gh_config_dir)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN");
    command
}

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
        .noprune(false)
        .build();

    for image in workspace_images(workspace_root).await? {
        docker
            .remove_image(&image, Some(options.clone()), None)
            .await?;
    }

    Ok(())
}

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

    docker
        .commit_container(commit_options, ContainerConfig::default())
        .await?;
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

    docker.commit_container(commit_options, config).await?;
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

    docker.commit_container(commit_options, config).await?;
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

    docker
        .commit_container(commit_options, ContainerConfig::default())
        .await?;
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

pub(crate) async fn remove_image_if_exists(image: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;

    if docker.inspect_image(image).await.is_err() {
        return Ok(());
    }

    let options = RemoveImageOptionsBuilder::default()
        .force(true)
        .noprune(false)
        .build();
    docker.remove_image(image, Some(options), None).await?;

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

pub(crate) fn workspace_id(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut id = String::with_capacity(12);

    for byte in digest.iter().take(6) {
        push_hex_byte(&mut id, *byte);
    }

    id
}

pub(crate) fn workspace_image_repository(root: &Path) -> String {
    let basename = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");

    format!(
        "decune/{}-{}",
        docker_name_segment(basename),
        workspace_id(root)
    )
}

pub(crate) fn write_fake_github_cli_feature_cache(
    workspace_root: &Path,
    cache_home: &Path,
    manifest_digest: &str,
    install_script: &str,
) {
    fs::create_dir_all(workspace_root.join(".decune")).unwrap();
    fs::write(
        workspace_root.join(".decune/features.lock.toml"),
        format!(
            r#"
version = 1

[[features]]
id = "ghcr.io/devcontainers/features/github-cli"
ref = "ghcr.io/devcontainers/features/github-cli:1"
digest = "{manifest_digest}"
"#
        ),
    )
    .unwrap();

    let cache_root = cache_home.join("decune/features");
    fs::create_dir_all(&cache_root).unwrap();
    let archive = cache_root.join(format!("{}.tgz", manifest_digest.replace(':', "_")));
    let metadata = r#"{"id":"github-cli","version":"1.0.0","name":"GitHub CLI"}"#;
    write_feature_archive(
        &archive,
        &[
            ("install.sh", install_script.as_bytes()),
            ("devcontainer-feature.json", metadata.as_bytes()),
        ],
    );
    let blob = fs::read(&archive).unwrap();
    let layer_digest = format!("sha256:{}", hex_lower(&Sha256::digest(&blob)));
    fs::write(
        archive.with_extension("tgz.toml"),
        format!("manifest_digest = \"{manifest_digest}\"\nlayer_digest = \"{layer_digest}\"\n"),
    )
    .unwrap();
}

fn write_feature_archive(path: &Path, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);
    for (path, content) in entries {
        let mut header = Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, *path, &mut &content[..])
            .unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    let mut file = encoder.finish().unwrap();
    file.flush().unwrap();
}

fn docker_name_segment(value: &str) -> String {
    let mut output = String::new();
    let mut previous_was_separator = true;

    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator {
            output.push('-');
            previous_was_separator = true;
        }
    }

    while output.ends_with('-') {
        output.pop();
    }

    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        push_hex_byte(&mut output, *byte);
    }
    output
}

pub(crate) fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub(crate) fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

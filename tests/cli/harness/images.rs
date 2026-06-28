use std::{collections::HashMap, fs, path::Path};

use super::{
    Docker,
    docker::{docker_status, ensure_alpine_image},
    locks::{acquire_exclusive_docker_resource_lock, acquire_shared_docker_resource_lock},
    names::{workspace_id, workspace_image_repository},
};

pub(crate) fn create_workspace_image_tag(
    workspace_root: &Path,
    tag: &str,
) -> anyhow::Result<String> {
    let docker = Docker::connect_with_defaults();
    ensure_alpine_image(&docker)?;

    let image_repository = workspace_image_repository(workspace_root);
    let image = format!("{image_repository}:{tag}");

    let _lock = acquire_shared_docker_resource_lock()?;
    docker_status(["tag", "alpine:3.20", &image])?;

    Ok(image)
}

pub(crate) fn create_image_without_devcontainer_metadata(image_tag: &str) -> anyhow::Result<()> {
    create_image_with_cmd(image_tag, Vec::new())
}

pub(crate) fn create_image_with_cmd(image_tag: &str, cmd: Vec<&str>) -> anyhow::Result<()> {
    if cmd.is_empty() {
        let docker = Docker::connect_with_defaults();
        ensure_alpine_image(&docker)?;
        let _lock = acquire_shared_docker_resource_lock()?;
        docker_status(["tag", "alpine:3.20", image_tag])?;
        return Ok(());
    }

    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            cmd: Some(cmd.into_iter().map(str::to_owned).collect()),
            ..DockerfileImageConfig::default()
        },
    )
}

pub(crate) fn create_image_with_github_cli(image_tag: &str) -> anyhow::Result<()> {
    let script = r#"
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
    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            run_script: Some(script.to_owned()),
            ..DockerfileImageConfig::default()
        },
    )
}

pub(crate) fn create_image_with_devcontainer_metadata_label(
    image_tag: &str,
    metadata: &str,
) -> anyhow::Result<()> {
    create_image_with_devcontainer_metadata_label_and_cmd(image_tag, metadata, vec!["true"])
}

pub(crate) fn create_image_with_devcontainer_metadata_label_and_cmd(
    image_tag: &str,
    metadata: &str,
    cmd: Vec<&str>,
) -> anyhow::Result<()> {
    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            labels: HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]),
            cmd: Some(cmd.into_iter().map(str::to_owned).collect()),
            ..DockerfileImageConfig::default()
        },
    )
}

pub(crate) fn create_nonroot_image_with_devcontainer_metadata_label_and_cmd(
    image_tag: &str,
    metadata: &str,
    cmd: Vec<&str>,
) -> anyhow::Result<()> {
    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            run_script: Some("adduser -D -u 20001 app\n".to_owned()),
            labels: HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]),
            cmd: Some(cmd.into_iter().map(str::to_owned).collect()),
            user: Some("app".to_owned()),
        },
    )
}

pub(crate) fn tag_image(source: &str, target: &str) -> anyhow::Result<()> {
    let _lock = acquire_shared_docker_resource_lock()?;
    docker_status(["tag", source, target])?;

    Ok(())
}

pub(crate) fn create_image_with_devcontainer_metadata(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    let script = r#"
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
    let metadata = r#"{"remoteUser":"devuser","remoteEnv":{"FROM_IMAGE":"label","EXPECTED_USER":"devuser"},"postStartCommand":"actual_user=$(id -un); expected_user=${EXPECTED_USER:-}; if [ \"$actual_user\" != \"$expected_user\" ]; then echo \"expected lifecycle user $expected_user, got $actual_user\" >&2; exit 11; fi; if [ \"${FROM_IMAGE:-}\" != \"label\" ]; then echo \"expected FROM_IMAGE=label, got ${FROM_IMAGE:-}\" >&2; exit 12; fi"}"#;
    _ = workspace_id(workspace_root);
    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            run_script: Some(script.to_owned()),
            labels: HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]),
            user: Some("root".to_owned()),
            ..DockerfileImageConfig::default()
        },
    )
}

pub(crate) fn create_image_with_nonstandard_home_user(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    _ = workspace_id(workspace_root);
    build_alpine_configured_image(
        image_tag,
        &DockerfileImageConfig {
            run_script: Some("adduser -D -h /usr/local/share/node node\n".to_owned()),
            ..DockerfileImageConfig::default()
        },
    )
}

pub(crate) fn remove_image_if_exists(image: &str) -> anyhow::Result<()> {
    let _lock = acquire_exclusive_docker_resource_lock()?;
    _ = docker_status(["image", "rm", "--force", "--no-prune", image]);

    Ok(())
}

#[derive(Debug, Clone, Default)]
struct DockerfileImageConfig {
    run_script: Option<String>,
    labels: HashMap<String, String>,
    cmd: Option<Vec<String>>,
    user: Option<String>,
}

fn build_alpine_configured_image(
    image_tag: &str,
    config: &DockerfileImageConfig,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults();
    ensure_alpine_image(&docker)?;

    let context = tempfile::tempdir()?;
    let dockerfile = dockerfile_for_config(config)?;
    fs::write(context.path().join("Dockerfile"), dockerfile)?;

    let _lock = acquire_shared_docker_resource_lock()?;
    docker_status([
        "build",
        "--tag",
        image_tag,
        "--file",
        context.path().join("Dockerfile").to_string_lossy().as_ref(),
        context.path().to_string_lossy().as_ref(),
    ])?;

    Ok(())
}

fn dockerfile_for_config(config: &DockerfileImageConfig) -> anyhow::Result<String> {
    let mut dockerfile = String::from("FROM alpine:3.20\n");
    if let Some(script) = &config.run_script {
        let script_command = format!("set -eu\n{script}");
        let run = vec!["/bin/sh", "-c", &script_command];
        dockerfile.push_str("RUN ");
        dockerfile.push_str(&serde_json::to_string(&run)?);
        dockerfile.push('\n');
    }
    for (key, value) in &config.labels {
        dockerfile.push_str("LABEL ");
        dockerfile.push_str(key);
        dockerfile.push('=');
        dockerfile.push_str(&dockerfile_label_value(value)?);
        dockerfile.push('\n');
    }
    if let Some(user) = &config.user {
        dockerfile.push_str("USER ");
        dockerfile.push_str(user);
        dockerfile.push('\n');
    }
    if let Some(cmd) = &config.cmd {
        dockerfile.push_str("CMD ");
        dockerfile.push_str(&serde_json::to_string(cmd)?);
        dockerfile.push('\n');
    }

    Ok(dockerfile)
}

fn dockerfile_label_value(value: &str) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?.replace('$', "\\$"))
}

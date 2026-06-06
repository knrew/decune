use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bollard::{
    body_full,
    models::BuildInfo,
    query_parameters::{BuildImageOptions, BuildImageOptionsBuilder},
};
use futures_util::StreamExt;

use crate::{
    config::{canonical::sha256_hex, hash::BuildHashInput, layer::LayerDevcontainerBuild},
    docker::{client::DockerClient, image::validate_image_name, lock::DockerResourceLock},
    ui,
};

const TAR_BLOCK_SIZE: usize = 512;
pub(crate) const FEATURE_ENTRYPOINT_WRAPPER: &str =
    "/usr/local/share/decune/feature-entrypoint-wrapper.sh";
pub(crate) const FEATURE_ENTRYPOINT_SENTINEL: &str = "/run/decune/feature-entrypoints-complete";
const FEATURE_ENTRYPOINTS_FILE: &str = "decune-feature-entrypoints";
const FEATURE_ENTRYPOINT_WRAPPER_FILE: &str = "decune-feature-entrypoint-wrapper.sh";
const FEATURE_ENTRYPOINTS_TARGET: &str = "/usr/local/share/decune/feature-entrypoints";
const UID_GID_SYNC_SCRIPT_FILE: &str = "sync-uid-gid.sh";
const UID_GID_SYNC_SCRIPT_TARGET: &str = "/tmp/decune-sync-uid-gid.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerBuildInput {
    pub(crate) image_tag: String,
    pub(crate) labels: std::collections::HashMap<String, String>,
    pub(crate) context: ResolvedBuildContext,
    pub(crate) options: DockerBuildOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DockerBuildOptions {
    pub(crate) build_args: BTreeMap<String, String>,
    pub(crate) target: Option<String>,
    pub(crate) cache_from: Vec<String>,
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedBuildContext {
    pub(crate) context_dir: PathBuf,
    pub(crate) dockerfile_path: PathBuf,
    pub(crate) dockerfile_in_context: PathBuf,
    pub(crate) dockerignore_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeatureLayerBuildInput {
    pub(crate) base_image: String,
    pub(crate) devcontainer_id: String,
    pub(crate) final_user: String,
    pub(crate) entrypoints: Vec<String>,
    pub(crate) install_env: BTreeMap<String, String>,
    pub(crate) context_dir: PathBuf,
    pub(crate) features: Vec<FeatureLayerBuildFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeatureLayerBuildFeature {
    pub(crate) id: String,
    pub(crate) source_dir: PathBuf,
    pub(crate) option_env: BTreeMap<String, String>,
    pub(crate) container_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UidGidSyncLayerBuildInput {
    pub(crate) base_image: String,
    pub(crate) final_user: String,
    pub(crate) target_user: String,
    pub(crate) old_uid: u32,
    pub(crate) old_gid: u32,
    pub(crate) new_uid: u32,
    pub(crate) new_gid: u32,
    pub(crate) context_dir: PathBuf,
}

pub(crate) async fn build_image(client: &DockerClient, input: DockerBuildInput) -> Result<()> {
    ui::info(&format!("Building Docker image: {}", input.image_tag));

    let tar = create_build_context_tar(&input.context)?;
    let options = build_image_options(&input);
    let _lock = DockerResourceLock::acquire_shared_from_env()?;
    let mut stream = client
        .raw()
        .build_image(options, None, Some(body_full(tar.into())));

    while let Some(item) = stream.next().await {
        let info =
            item.with_context(|| format!("Failed to build Docker image: {}", input.image_tag))?;
        handle_build_info(&input.image_tag, info)?;
    }

    ui::done(&format!("Built Docker image: {}", input.image_tag));
    Ok(())
}

pub(crate) fn resolve_build_context(
    _workspace_root: &Path,
    devcontainer_file: &Path,
    build: &LayerDevcontainerBuild,
) -> Result<ResolvedBuildContext> {
    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Devcontainer path has no parent: {}",
            devcontainer_file.display()
        )
    })?;
    let context_dir = resolve_existing_dir(
        devcontainer_dir,
        build.context.as_deref().unwrap_or("."),
        "Docker build context",
    )?;
    let dockerfile_path = resolve_existing_file(devcontainer_dir, &build.dockerfile, "Dockerfile")?;
    let dockerfile_in_context = dockerfile_path
        .strip_prefix(&context_dir)
        .with_context(|| {
            format!(
                "Dockerfile must be inside build context: {} is outside {}",
                dockerfile_path.display(),
                context_dir.display()
            )
        })?
        .to_path_buf();
    let dockerignore = context_dir.join(".dockerignore");
    let dockerignore_path = if dockerignore.is_file() {
        Some(dockerignore)
    } else {
        None
    };

    Ok(ResolvedBuildContext {
        context_dir,
        dockerfile_path,
        dockerfile_in_context,
        dockerignore_path,
    })
}

pub(crate) fn create_build_context_tar(context: &ResolvedBuildContext) -> Result<Vec<u8>> {
    let rules = DockerignoreRules::load(context.dockerignore_path.as_deref())?;
    let mut output = Vec::new();
    let mut entries = Vec::new();
    collect_context_entries(
        &context.context_dir,
        &context.context_dir,
        &rules,
        &mut entries,
    )?;
    entries.push(context.dockerfile_in_context.clone());
    if let Some(dockerignore_path) = &context.dockerignore_path {
        let dockerignore_in_context = dockerignore_path
            .strip_prefix(&context.context_dir)
            .with_context(|| {
                format!(
                    ".dockerignore must be inside build context: {} is outside {}",
                    dockerignore_path.display(),
                    context.context_dir.display()
                )
            })?
            .to_path_buf();
        entries.push(dockerignore_in_context);
    }

    entries.sort();
    entries.dedup();
    for relative_path in entries {
        append_tar_entry(&mut output, &context.context_dir, &relative_path)?;
    }

    output.extend([0; TAR_BLOCK_SIZE * 2]);
    Ok(output)
}

pub(crate) fn prepare_feature_layer_build_context(
    input: &FeatureLayerBuildInput,
) -> Result<ResolvedBuildContext> {
    if input.context_dir.exists() {
        fs::remove_dir_all(&input.context_dir).with_context(|| {
            format!(
                "Failed to remove existing Feature build context: {}",
                input.context_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&input.context_dir).with_context(|| {
        format!(
            "Failed to create Feature build context: {}",
            input.context_dir.display()
        )
    })?;

    let dockerfile_path = input.context_dir.join("Dockerfile");
    fs::write(&dockerfile_path, feature_layer_dockerfile(input)?).with_context(|| {
        format!(
            "Failed to write Feature layer Dockerfile: {}",
            dockerfile_path.display()
        )
    })?;
    let install_script_path = input.context_dir.join("install-features.sh");
    fs::write(&install_script_path, feature_layer_install_script(input)?).with_context(|| {
        format!(
            "Failed to write Feature layer install script: {}",
            install_script_path.display()
        )
    })?;
    let entrypoints_path = input.context_dir.join(FEATURE_ENTRYPOINTS_FILE);
    fs::write(
        &entrypoints_path,
        feature_entrypoints_file(&input.entrypoints, &input.devcontainer_id)?,
    )
    .with_context(|| {
        format!(
            "Failed to write Feature entrypoints file: {}",
            entrypoints_path.display()
        )
    })?;
    let wrapper_path = input.context_dir.join(FEATURE_ENTRYPOINT_WRAPPER_FILE);
    fs::write(&wrapper_path, feature_entrypoint_wrapper()).with_context(|| {
        format!(
            "Failed to write Feature entrypoint wrapper: {}",
            wrapper_path.display()
        )
    })?;

    for (index, feature) in input.features.iter().enumerate() {
        let feature_dir = input
            .context_dir
            .join(feature_context_name(index, &feature.id));
        copy_directory(&feature.source_dir, &feature_dir).with_context(|| {
            format!(
                "Failed to stage Feature files from {}",
                feature.source_dir.display()
            )
        })?;
        let env_path = feature_dir.join("devcontainer-features.env");
        let env_content = feature_env_file(input, feature).map_err(|error| {
            anyhow::anyhow!(
                "Failed to prepare Feature option env file for {} at {}: {error}",
                feature.id,
                env_path.display()
            )
        })?;
        fs::write(&env_path, env_content).with_context(|| {
            format!(
                "Failed to write Feature option env file: {}",
                env_path.display()
            )
        })?;
    }

    Ok(ResolvedBuildContext {
        context_dir: input.context_dir.clone(),
        dockerfile_path,
        dockerfile_in_context: PathBuf::from("Dockerfile"),
        dockerignore_path: None,
    })
}

pub(crate) fn prepare_uid_gid_sync_layer_build_context(
    input: &UidGidSyncLayerBuildInput,
) -> Result<ResolvedBuildContext> {
    if input.context_dir.exists() {
        fs::remove_dir_all(&input.context_dir).with_context(|| {
            format!(
                "Failed to remove existing UID/GID sync build context: {}",
                input.context_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&input.context_dir).with_context(|| {
        format!(
            "Failed to create UID/GID sync build context: {}",
            input.context_dir.display()
        )
    })?;

    let dockerfile_path = input.context_dir.join("Dockerfile");
    fs::write(&dockerfile_path, uid_gid_sync_layer_dockerfile(input)?).with_context(|| {
        format!(
            "Failed to write UID/GID sync Dockerfile: {}",
            dockerfile_path.display()
        )
    })?;
    let script_path = input.context_dir.join(UID_GID_SYNC_SCRIPT_FILE);
    fs::write(&script_path, uid_gid_sync_script(input)?).with_context(|| {
        format!(
            "Failed to write UID/GID sync script: {}",
            script_path.display()
        )
    })?;

    Ok(ResolvedBuildContext {
        context_dir: input.context_dir.clone(),
        dockerfile_path,
        dockerfile_in_context: PathBuf::from("Dockerfile"),
        dockerignore_path: None,
    })
}

pub(crate) fn build_hash_input(context: &ResolvedBuildContext) -> Result<BuildHashInput> {
    let dockerfile = fs::read(&context.dockerfile_path).with_context(|| {
        format!(
            "Failed to read Dockerfile for config hash: {}",
            context.dockerfile_path.display()
        )
    })?;
    let dockerignore_content_hash = match &context.dockerignore_path {
        Some(path) => {
            let contents = fs::read(path).with_context(|| {
                format!(
                    "Failed to read .dockerignore for config hash: {}",
                    path.display()
                )
            })?;
            Some(sha256_hex(&contents))
        }
        None => None,
    };

    Ok(BuildHashInput {
        dockerfile_path: Some(context.dockerfile_path.display().to_string()),
        dockerfile_content_hash: Some(sha256_hex(&dockerfile)),
        context_path: Some(context.context_dir.display().to_string()),
        dockerignore_content_hash,
    })
}

#[cfg(test)]
pub(crate) fn tar_contains_path(tar: &[u8], path: &str) -> bool {
    let mut offset = 0;
    while offset + TAR_BLOCK_SIZE <= tar.len() {
        let header = &tar[offset..offset + TAR_BLOCK_SIZE];
        if header.iter().all(|byte| *byte == 0) {
            return false;
        }

        if tar_header_path(header).as_deref() == Some(path) {
            return true;
        }

        let size = parse_tar_octal(&header[124..136]).unwrap_or(0);
        offset += TAR_BLOCK_SIZE + padded_size(size);
    }

    false
}

fn build_image_options(input: &DockerBuildInput) -> BuildImageOptions {
    let labels = input.labels.clone();
    let mut builder = BuildImageOptionsBuilder::default()
        .dockerfile(&path_for_docker(&input.context.dockerfile_in_context))
        .t(&input.image_tag)
        .labels(&labels)
        .rm(true)
        .forcerm(true)
        .nocache(input.options.no_cache);

    if !input.options.build_args.is_empty() {
        let build_args = input
            .options
            .build_args
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        builder = builder.buildargs(&build_args);
    }

    if let Some(target) = &input.options.target {
        builder = builder.target(target);
    }

    if !input.options.cache_from.is_empty() {
        builder = builder.cachefrom(&input.options.cache_from);
    }

    if input.options.pull {
        builder = builder.pull("true");
    }

    builder.build()
}

fn handle_build_info(image_tag: &str, info: BuildInfo) -> Result<()> {
    if let Some(error) = info.error_detail.and_then(|detail| detail.message) {
        bail!("Failed to build Docker image: {image_tag}: {error}");
    }

    if let Some(stream) = info.stream {
        let line = stream.trim();
        if !line.is_empty() {
            ui::info(line);
        }
    }

    Ok(())
}

fn resolve_existing_dir(base: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = resolve_path(base, value);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {label}: {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} must be a directory: {}", canonical.display());
    }

    Ok(canonical)
}

fn resolve_existing_file(base: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = resolve_path(base, value);
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {label}: {}", path.display()))?;
    if !canonical.is_file() {
        bail!("{label} must be a file: {}", canonical.display());
    }

    Ok(canonical)
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn feature_layer_dockerfile(input: &FeatureLayerBuildInput) -> Result<String> {
    validate_image_name(&input.base_image)?;
    let final_user = dockerfile_user(&input.final_user)?;
    Ok(format!(
        "FROM {}\nUSER root\nRUN mkdir -p /usr/local/share/decune\nCOPY {FEATURE_ENTRYPOINT_WRAPPER_FILE} {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY {FEATURE_ENTRYPOINTS_FILE} {FEATURE_ENTRYPOINTS_TARGET}\nRUN chmod +x {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY . /tmp/decune-features/\nRUN /bin/sh /tmp/decune-features/install-features.sh\nUSER {final_user}\n",
        input.base_image
    ))
}

fn uid_gid_sync_layer_dockerfile(input: &UidGidSyncLayerBuildInput) -> Result<String> {
    validate_image_name(&input.base_image)?;
    let final_user = dockerfile_user(&input.final_user)?;
    Ok(format!(
        "FROM {}\nUSER root\nCOPY {UID_GID_SYNC_SCRIPT_FILE} {UID_GID_SYNC_SCRIPT_TARGET}\nRUN /bin/sh {UID_GID_SYNC_SCRIPT_TARGET} && rm -f {UID_GID_SYNC_SCRIPT_TARGET}\nUSER {final_user}\n",
        input.base_image
    ))
}

fn uid_gid_sync_script(input: &UidGidSyncLayerBuildInput) -> Result<String> {
    let target_user = shell_quote(&input.target_user);
    Ok(format!(
        r#"set -eu
target_user={target_user}
old_uid={old_uid}
old_gid={old_gid}
new_uid={new_uid}
new_gid={new_gid}

if [ "$old_uid" = "$new_uid" ] && [ "$old_gid" = "$new_gid" ]; then
    exit 0
fi

conflict_user="$(awk -F: -v uid="$new_uid" -v user="$target_user" '$3 == uid && $1 != user {{ print $1; exit }}' /etc/passwd)"
if [ -n "$conflict_user" ]; then
    echo "UID/GID sync target UID conflicts with existing user: $conflict_user ($new_uid)" >&2
    exit 1
fi

target_home="$(awk -F: -v user="$target_user" '$1 == user {{ print $6; exit }}' /etc/passwd)"
tmp_passwd="$(mktemp)"
status=0
awk -F: -v OFS=: -v user="$target_user" -v uid="$new_uid" -v gid="$new_gid" '
    $1 == user {{ $3 = uid; $4 = gid; found = 1 }}
    {{ print }}
    END {{ if (!found) exit 42 }}
' /etc/passwd > "$tmp_passwd" || status=$?
if [ "$status" -eq 42 ]; then
    echo "UID/GID sync target user is missing: $target_user" >&2
    exit 1
elif [ "$status" -ne 0 ]; then
    exit "$status"
fi
cat "$tmp_passwd" >/etc/passwd
rm -f "$tmp_passwd"

if [ -f /etc/group ]; then
    target_group="$(awk -F: -v gid="$old_gid" '$3 == gid {{ print $1; exit }}' /etc/group)"
    conflict_group="$(awk -F: -v gid="$new_gid" -v group="$target_group" '$3 == gid && (group == "" || $1 != group) {{ print $1; exit }}' /etc/group)"
    if [ -n "$conflict_group" ]; then
        echo "UID/GID sync target GID conflicts with existing group: $conflict_group ($new_gid)" >&2
        exit 1
    fi
    if [ -n "$target_group" ]; then
        tmp_group="$(mktemp)"
        awk -F: -v OFS=: -v group="$target_group" -v gid="$new_gid" '
            $1 == group {{ $3 = gid }}
            {{ print }}
        ' /etc/group > "$tmp_group"
        cat "$tmp_group" >/etc/group
        rm -f "$tmp_group"
    fi
fi

if [ -n "$target_home" ] && [ -d "$target_home" ]; then
    chown -R "$new_uid:$new_gid" "$target_home"
fi
"#,
        target_user = target_user,
        old_uid = input.old_uid,
        old_gid = input.old_gid,
        new_uid = input.new_uid,
        new_gid = input.new_gid,
    ))
}

fn feature_layer_install_script(input: &FeatureLayerBuildInput) -> Result<String> {
    let mut script = String::new();
    script.push_str("set -eu\n");
    script.push_str(
        r#"decune_feature_user_home() {
    user="${1:-}"
    user="${user%%:*}"
    if [ -z "$user" ]; then
        return 0
    fi
    if command -v getent >/dev/null 2>&1; then
        record="$(getent passwd "$user" || true)"
        if [ -n "$record" ]; then
            printf '%s\n' "$record" | cut -d: -f6
            return 0
        fi
    fi
    while IFS=: read -r name passwd uid gid gecos home shell; do
        if [ "$name" = "$user" ] || [ "$uid" = "$user" ]; then
            printf '%s\n' "$home"
            return 0
        fi
    done </etc/passwd
    return 0
}
"#,
    );
    for (index, feature) in input.features.iter().enumerate() {
        let name = feature_context_name(index, &feature.id);
        script.push_str(&format!(
            "(\nset -a\n. /tmp/decune-features/{name}/devcontainer-features.env\n_CONTAINER_USER_HOME=\"$(decune_feature_user_home \"${{_CONTAINER_USER:-}}\")\"\n_REMOTE_USER_HOME=\"$(decune_feature_user_home \"${{_REMOTE_USER:-}}\")\"\nexport _CONTAINER_USER_HOME _REMOTE_USER_HOME\nset +a\n"
        ));
        script.push_str(&format!(
            "chmod +x /tmp/decune-features/{name}/install.sh\ncd /tmp/decune-features/{name}\n./install.sh\n)\n"
        ));
    }
    script.push_str("rm -rf /tmp/decune-features\n");
    Ok(script)
}

fn feature_entrypoints_file(entrypoints: &[String], devcontainer_id: &str) -> Result<String> {
    let mut output = String::new();
    for entrypoint in entrypoints {
        let entrypoint = entrypoint.replace("${devcontainerId}", devcontainer_id);
        if entrypoint
            .chars()
            .any(|character| character == '\0' || character == '\n' || character == '\r')
        {
            bail!("Feature entrypoint contains unsupported control characters");
        }
        output.push_str(&entrypoint);
        output.push('\n');
    }
    Ok(output)
}

fn feature_entrypoint_wrapper() -> &'static str {
    r#"#!/bin/sh
set -eu
feature_startup_id() {
    stat_line=$(cat /proc/1/stat 2>/dev/null || true)
    stat_tail=${stat_line#*) }
    set -- $stat_tail
    printf '%s' "${20:-unknown}"
}
sentinel=/run/decune/feature-entrypoints-complete
sentinel_startup_id=$(feature_startup_id)
: > "$sentinel"
if [ -f /usr/local/share/decune/feature-entrypoints ]; then
    while IFS= read -r entrypoint; do
        if [ -n "$entrypoint" ]; then
            /bin/sh -c "$entrypoint"
        fi
    done </usr/local/share/decune/feature-entrypoints
fi
printf '%s\n' "$sentinel_startup_id" > "$sentinel"
if [ "$#" -eq 0 ]; then
    trap 'exit 0' TERM
    while sleep 1 & wait $!; do :; done
fi
exec "$@"
"#
}

fn dockerfile_user(user: &str) -> Result<&str> {
    let trimmed = user.trim();
    if trimmed.is_empty() || trimmed != user {
        bail!("Docker image user must not be empty or contain surrounding whitespace");
    }
    if user
        .chars()
        .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        bail!("Docker image user contains unsupported whitespace or control characters: {user}");
    }

    Ok(user)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn feature_env_file(
    input: &FeatureLayerBuildInput,
    feature: &FeatureLayerBuildFeature,
) -> Result<String> {
    let mut env = feature.container_env.clone();
    env.extend(feature.option_env.clone());
    env.extend(input.install_env.clone());

    let mut output = String::new();
    for (key, value) in env {
        validate_feature_env_key(&feature.id, &key)?;
        output.push_str(&format!("{key}={}\n", shell_single_quote(&value)));
    }

    Ok(output)
}

fn validate_feature_env_key(feature_id: &str, key: &str) -> Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        bail!("Feature environment variable name is not supported for {feature_id}: empty name");
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || chars.any(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        bail!("Feature environment variable name is not supported for {feature_id}: {key}");
    }

    Ok(())
}

fn feature_context_name(index: usize, id: &str) -> String {
    let mut name = String::new();
    for character in id.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    while name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() {
        name.push_str("feature");
    }

    format!("{index:03}-{name}")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| {
        format!(
            "Failed to create staged Feature directory: {}",
            destination.display()
        )
    })?;
    let mut entries = fs::read_dir(source)
        .with_context(|| format!("Failed to read Feature directory: {}", source.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to enumerate Feature directory: {}",
                source.display()
            )
        })?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).with_context(|| {
            format!("Failed to inspect Feature file: {}", source_path.display())
        })?;
        if metadata.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path).with_context(|| {
                format!("Failed to read Feature symlink: {}", source_path.display())
            })?;
            std::os::unix::fs::symlink(&target, &destination_path).with_context(|| {
                format!(
                    "Failed to stage Feature symlink {} -> {}",
                    destination_path.display(),
                    target.display()
                )
            })?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "Failed to copy Feature file {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            fs::set_permissions(&destination_path, metadata.permissions()).with_context(|| {
                format!(
                    "Failed to preserve Feature file permissions: {}",
                    destination_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn collect_context_entries(
    context_dir: &Path,
    directory: &Path,
    rules: &DockerignoreRules,
    entries: &mut Vec<PathBuf>,
) -> Result<()> {
    let read_dir = fs::read_dir(directory).with_context(|| {
        format!(
            "Failed to read Docker build context: {}",
            directory.display()
        )
    })?;
    let mut children = read_dir
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to enumerate Docker build context: {}",
                directory.display()
            )
        })?;
    children.sort_by_key(|entry| entry.path());

    for child in children {
        let path = child.path();
        let relative_path = path.strip_prefix(context_dir).with_context(|| {
            format!(
                "Failed to relativize Docker build context path: {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect build context path: {}", path.display()))?;
        let is_dir = metadata.is_dir();

        if !rules.is_ignored(relative_path) {
            entries.push(relative_path.to_path_buf());
        }

        if is_dir {
            collect_context_entries(context_dir, &path, rules, entries)?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerignoreRules {
    rules: Vec<DockerignoreRule>,
}

impl DockerignoreRules {
    fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self { rules: Vec::new() });
        };
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read .dockerignore: {}", path.display()))?;
        Ok(Self::parse(&contents))
    }

    fn parse(contents: &str) -> Self {
        let rules = contents
            .lines()
            .filter_map(DockerignoreRule::parse)
            .collect();

        Self { rules }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        let path = path_for_docker(path);
        self.rules
            .iter()
            .filter(|rule| rule.matches(&path))
            .fold(false, |_, rule| !rule.negated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerignoreRule {
    pattern: String,
    negated: bool,
}

impl DockerignoreRule {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches('\r');
        if line.starts_with('#') {
            return None;
        }

        let mut line = line.trim();
        if line.is_empty() {
            return None;
        }

        let negated = line.starts_with('!');
        if negated {
            line = line[1..].trim_start();
        }

        let line = line.trim_start_matches('/');
        let pattern = line.trim_end_matches('/').to_owned();
        if pattern.is_empty() || pattern == "." {
            return None;
        }

        Some(Self { pattern, negated })
    }

    fn matches(&self, path: &str) -> bool {
        if glob_match(&self.pattern, path) {
            return true;
        }

        let mut parent = path;
        while let Some((next_parent, _)) = parent.rsplit_once('/') {
            if glob_match(&self.pattern, next_parent) {
                return true;
            }
            parent = next_parent;
        }

        false
    }
}

fn append_tar_entry(output: &mut Vec<u8>, context_dir: &Path, relative_path: &Path) -> Result<()> {
    let path = context_dir.join(relative_path);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Failed to inspect build context path: {}", path.display()))?;
    let name = path_for_docker(relative_path);

    if metadata.is_dir() {
        append_tar_header(output, &name, &metadata, 0, b'5', None)?;
        return Ok(());
    }

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&path).with_context(|| {
            format!(
                "Failed to read symlink in build context: {}",
                path.display()
            )
        })?;
        append_tar_header(
            output,
            &name,
            &metadata,
            0,
            b'2',
            Some(target.to_string_lossy().as_ref()),
        )?;
        return Ok(());
    }

    if metadata.is_file() {
        append_tar_header(output, &name, &metadata, metadata.len(), b'0', None)?;
        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to read build context file: {}", path.display()))?;
        file.read_to_end(output)
            .with_context(|| format!("Failed to archive build context file: {}", path.display()))?;
        pad_tar(output);
    }

    Ok(())
}

fn append_tar_header(
    output: &mut Vec<u8>,
    name: &str,
    metadata: &fs::Metadata,
    size: u64,
    entry_type: u8,
    link_name: Option<&str>,
) -> Result<()> {
    let mut header = [0u8; TAR_BLOCK_SIZE];
    let (name, prefix) = split_tar_name(name)?;
    write_tar_bytes(&mut header[0..100], name.as_bytes());
    write_tar_octal(
        &mut header[100..108],
        metadata.permissions().mode() as u64 & 0o7777,
    );
    write_tar_octal(&mut header[108..116], 0);
    write_tar_octal(&mut header[116..124], 0);
    write_tar_octal(&mut header[124..136], size);
    write_tar_octal(
        &mut header[136..148],
        metadata
            .modified()
            .ok()
            .and_then(|time| {
                time.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_secs())
            })
            .unwrap_or(0),
    );
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    header[156] = entry_type;
    if let Some(link_name) = link_name {
        write_tar_bytes(&mut header[157..257], link_name.as_bytes());
    }
    write_tar_bytes(&mut header[257..263], b"ustar\0");
    write_tar_bytes(&mut header[263..265], b"00");
    if let Some(prefix) = prefix {
        write_tar_bytes(&mut header[345..500], prefix.as_bytes());
    }
    let checksum = header.iter().map(|byte| *byte as u32).sum::<u32>();
    write_tar_checksum(&mut header[148..156], checksum);
    output.extend(header);

    Ok(())
}

fn split_tar_name(path: &str) -> Result<(&str, Option<&str>)> {
    if path.len() <= 100 {
        return Ok((path, None));
    }

    for index in path.match_indices('/').map(|(index, _)| index).rev() {
        let prefix = &path[..index];
        let name = &path[index + 1..];
        if prefix.len() <= 155 && name.len() <= 100 {
            return Ok((name, Some(prefix)));
        }
    }

    bail!("Build context path is too long for tar header: {path}");
}

fn write_tar_bytes(target: &mut [u8], value: &[u8]) {
    let len = target.len().min(value.len());
    target[..len].copy_from_slice(&value[..len]);
}

fn write_tar_octal(target: &mut [u8], value: u64) {
    let width = target.len() - 1;
    let text = format!("{value:0width$o}");
    write_tar_bytes(&mut target[..width], text.as_bytes());
    target[width] = 0;
}

fn write_tar_checksum(target: &mut [u8], value: u32) {
    let text = format!("{value:06o}\0 ");
    write_tar_bytes(target, text.as_bytes());
}

fn pad_tar(output: &mut Vec<u8>) {
    let remainder = output.len() % TAR_BLOCK_SIZE;
    if remainder != 0 {
        output.extend(vec![0; TAR_BLOCK_SIZE - remainder]);
    }
}

#[cfg(test)]
fn tar_header_path(header: &[u8]) -> Option<String> {
    let name = tar_string(&header[0..100])?;
    let prefix = tar_string(&header[345..500]);
    Some(match prefix {
        Some(prefix) => format!("{prefix}/{name}"),
        None => name,
    })
}

#[cfg(test)]
fn tar_string(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if end == 0 {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes[..end]).to_string())
    }
}

#[cfg(test)]
fn parse_tar_octal(bytes: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(bytes);
    usize::from_str_radix(text.trim_matches(char::from(0)).trim(), 8).ok()
}

#[cfg(test)]
fn padded_size(size: usize) -> usize {
    size.div_ceil(TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
}

fn path_for_docker(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&b'*', rest)) => {
            if let Some((&b'*', rest)) = rest.split_first() {
                let matches_zero_directories = rest
                    .strip_prefix(b"/")
                    .is_some_and(|rest| glob_match_bytes(rest, text));

                if matches_zero_directories {
                    return true;
                }

                glob_match_bytes(rest, text)
                    || (!text.is_empty() && glob_match_bytes(pattern, &text[1..]))
            } else {
                glob_match_bytes(rest, text)
                    || (!text.is_empty()
                        && text[0] != b'/'
                        && glob_match_bytes(pattern, &text[1..]))
            }
        }
        Some((&b'?', rest)) => {
            !text.is_empty() && text[0] != b'/' && glob_match_bytes(rest, &text[1..])
        }
        Some((&expected, rest)) => {
            !text.is_empty() && text[0] == expected && glob_match_bytes(rest, &text[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        fs,
        path::Path,
    };

    use crate::config::layer::LayerDevcontainerBuild;
    use tempfile::TempDir;

    use super::{
        DockerBuildInput, DockerBuildOptions, FEATURE_ENTRYPOINT_SENTINEL,
        FEATURE_ENTRYPOINT_WRAPPER, FEATURE_ENTRYPOINT_WRAPPER_FILE, FEATURE_ENTRYPOINTS_FILE,
        FeatureLayerBuildFeature, FeatureLayerBuildInput, UID_GID_SYNC_SCRIPT_FILE,
        UidGidSyncLayerBuildInput, build_image_options, create_build_context_tar,
        prepare_feature_layer_build_context, prepare_uid_gid_sync_layer_build_context,
        resolve_build_context, tar_contains_path,
    };

    #[test]
    fn build_context_defaults_to_devcontainer_directory() {
        let temp = tempdir("default-context");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::write(root.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        assert_eq!(context.context_dir, root.join(".devcontainer"));
        assert_eq!(
            context.dockerfile_path,
            root.join(".devcontainer/Dockerfile")
        );
        assert_eq!(context.dockerfile_in_context, Path::new("Dockerfile"));
    }

    #[test]
    fn build_context_and_dockerfile_are_resolved_relative_to_devcontainer_file() {
        let temp = tempdir("relative-context");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/config/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join(".devcontainer/docker")).unwrap();
        fs::write(
            root.join(".devcontainer/docker/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "../docker/Dockerfile".to_owned(),
            context: Some("..".to_owned()),
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        assert_eq!(context.context_dir, root.join(".devcontainer"));
        assert_eq!(
            context.dockerfile_path,
            root.join(".devcontainer/docker/Dockerfile")
        );
        assert_eq!(
            context.dockerfile_in_context,
            Path::new("docker/Dockerfile")
        );
    }

    #[test]
    fn dockerfile_outside_context_is_rejected() {
        let temp = tempdir("outside-dockerfile");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        fs::create_dir_all(devcontainer_file.parent().unwrap()).unwrap();
        fs::write(root.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "../.devcontainer/Dockerfile".to_owned(),
            context: Some("../app".to_owned()),
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };

        let error = resolve_build_context(&root, &devcontainer_file, &build).unwrap_err();

        assert!(error.to_string().contains("inside build context"));
    }

    #[test]
    fn dockerignore_excludes_files_from_tar_context() {
        let temp = tempdir("dockerignore-tar");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("tmp")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("app.txt"), "included-content\n").unwrap();
        fs::write(context_dir.join("secret.env"), "excluded-secret\n").unwrap();
        fs::write(context_dir.join("tmp/cache.txt"), "excluded-cache\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "*.env\ntmp/\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, "app.txt"));
        assert!(tar_contains_path(&tar, ".dockerignore"));
        assert!(!tar_contains_path(&tar, "secret.env"));
        assert!(!tar_contains_path(&tar, "tmp/cache.txt"));
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("included-content"));
        assert!(!text.contains("excluded-secret"));
        assert!(!text.contains("excluded-cache"));
    }

    #[test]
    fn build_image_options_include_devcontainer_build_options() {
        let input = docker_build_input(DockerBuildOptions {
            build_args: [("VARIANT".to_owned(), "bookworm".to_owned())].into(),
            target: Some("dev".to_owned()),
            cache_from: vec!["type=registry,ref=example.test/cache:latest".to_owned()],
            no_cache: true,
            pull: true,
        });

        let options = build_image_options(&input);

        assert_eq!(
            options.buildargs,
            Some(HashMap::from([(
                "VARIANT".to_owned(),
                "bookworm".to_owned()
            )]))
        );
        assert_eq!(options.target, "dev");
        assert_eq!(
            options.cachefrom,
            Some(vec![
                "type=registry,ref=example.test/cache:latest".to_owned()
            ])
        );
        assert!(options.nocache);
        assert_eq!(options.pull.as_deref(), Some("true"));
    }

    #[test]
    fn build_image_options_omit_empty_optional_build_options() {
        let input = docker_build_input(DockerBuildOptions::default());

        let options = build_image_options(&input);

        assert_eq!(options.buildargs, None);
        assert_eq!(options.target, "");
        assert_eq!(options.cachefrom, None);
        assert!(!options.nocache);
        assert_eq!(options.pull, None);
    }

    #[test]
    fn dockerignore_negation_reincludes_later_matches() {
        let temp = tempdir("dockerignore-negation");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("tmp")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("tmp/cache.txt"), "excluded-cache\n").unwrap();
        fs::write(context_dir.join("tmp/keep.txt"), "included-keep\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "tmp/*\n!tmp/keep.txt\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "tmp/cache.txt"));
        assert!(tar_contains_path(&tar, "tmp/keep.txt"));
    }

    #[test]
    fn dockerignore_keeps_build_metadata_when_ignore_rule_matches_everything() {
        let temp = tempdir("dockerignore-build-metadata");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("app.txt"), "excluded-content\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "*\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, ".dockerignore"));
        assert!(!tar_contains_path(&tar, "app.txt"));
    }

    #[test]
    fn feature_layer_build_context_stages_features_env_and_cleanup_dockerfile() {
        let temp = tempdir("feature-layer-build-context");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            source.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        let context_dir = temp.path().join("context");
        let context = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
            base_image: "alpine:3.20".to_owned(),
            devcontainer_id: "workspace-id".to_owned(),
            final_user: "vscode".to_owned(),
            entrypoints: vec!["touch /tmp/feature-${devcontainerId}".to_owned()],
            install_env: BTreeMap::from([
                ("_CONTAINER_USER".to_owned(), "root".to_owned()),
                ("_CONTAINER_USER_HOME".to_owned(), "/root".to_owned()),
                ("_REMOTE_USER".to_owned(), "vscode".to_owned()),
                ("_REMOTE_USER_HOME".to_owned(), "/home/vscode".to_owned()),
            ]),
            context_dir,
            features: vec![FeatureLayerBuildFeature {
                id: "ghcr.io/example/features/tool".to_owned(),
                source_dir: source,
                option_env: BTreeMap::from([(
                    "VERSION".to_owned(),
                    "1.2'$(echo unsafe)".to_owned(),
                )]),
                container_env: BTreeMap::new(),
            }],
        })
        .unwrap();

        let tar = create_build_context_tar(&context).unwrap();
        let dockerfile = fs::read_to_string(context.dockerfile_path).unwrap();
        let install_script =
            fs::read_to_string(context.context_dir.join("install-features.sh")).unwrap();
        let entrypoints =
            fs::read_to_string(context.context_dir.join(FEATURE_ENTRYPOINTS_FILE)).unwrap();
        let wrapper =
            fs::read_to_string(context.context_dir.join(FEATURE_ENTRYPOINT_WRAPPER_FILE)).unwrap();

        assert!(tar_contains_path(
            &tar,
            "000-ghcr-io-example-features-tool/install.sh"
        ));
        assert!(tar_contains_path(
            &tar,
            "000-ghcr-io-example-features-tool/devcontainer-features.env"
        ));
        assert!(dockerfile.contains("FROM alpine:3.20"));
        assert!(dockerfile.contains("/bin/sh /tmp/decune-features/install-features.sh"));
        assert!(dockerfile.contains(FEATURE_ENTRYPOINT_WRAPPER));
        assert!(dockerfile.contains("USER vscode"));
        assert!(wrapper.contains(FEATURE_ENTRYPOINT_SENTINEL));
        assert!(wrapper.contains("sentinel_startup_id=$(feature_startup_id)"));
        assert!(wrapper.contains(": > \"$sentinel\""));
        assert!(!wrapper.contains("rm -f \"$sentinel\""));
        assert!(!wrapper.contains("mkdir -p /run/decune"));
        assert!(install_script.contains("./install.sh"));
        assert!(!install_script.contains("/bin/sh ./install.sh"));
        assert!(install_script.contains("rm -rf /tmp/decune-features"));
        assert_eq!(entrypoints, "touch /tmp/feature-workspace-id\n");
        assert_eq!(
            fs::read_to_string(
                context
                    .context_dir
                    .join("000-ghcr-io-example-features-tool/devcontainer-features.env")
            )
            .unwrap(),
            "VERSION='1.2'\"'\"'$(echo unsafe)'\n_CONTAINER_USER='root'\n_CONTAINER_USER_HOME='/root'\n_REMOTE_USER='vscode'\n_REMOTE_USER_HOME='/home/vscode'\n"
        );
    }

    #[test]
    fn uid_gid_sync_layer_build_context_writes_sync_dockerfile_and_script() {
        let temp = tempdir("uid-gid-sync-layer-build-context");
        let context_dir = temp.path().join("context");
        let context = prepare_uid_gid_sync_layer_build_context(&UidGidSyncLayerBuildInput {
            base_image: "alpine:3.20".to_owned(),
            final_user: "vscode".to_owned(),
            target_user: "vscode".to_owned(),
            old_uid: 2001,
            old_gid: 2001,
            new_uid: 1000,
            new_gid: 1000,
            context_dir,
        })
        .unwrap();

        let tar = create_build_context_tar(&context).unwrap();
        let dockerfile = fs::read_to_string(context.dockerfile_path).unwrap();
        let script =
            fs::read_to_string(context.context_dir.join(UID_GID_SYNC_SCRIPT_FILE)).unwrap();

        assert!(tar_contains_path(&tar, "Dockerfile"));
        assert!(tar_contains_path(&tar, UID_GID_SYNC_SCRIPT_FILE));
        assert!(dockerfile.contains("FROM alpine:3.20"));
        assert!(dockerfile.contains("USER root"));
        assert!(dockerfile.contains("USER vscode"));
        assert!(script.contains("target_user='vscode'"));
        assert!(script.contains("UID/GID sync target UID conflicts"));
        assert!(script.contains("UID/GID sync target GID conflicts"));
        assert!(script.contains("cat \"$tmp_passwd\" >/etc/passwd"));
        assert!(script.contains("cat \"$tmp_group\" >/etc/group"));
        assert!(script.contains("chown -R \"$new_uid:$new_gid\" \"$target_home\""));
    }

    #[test]
    fn feature_layer_build_context_rejects_invalid_feature_env_key() {
        let temp = tempdir("feature-layer-build-context-invalid-env");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            source.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        let context_dir = temp.path().join("context");

        let error = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
            base_image: "alpine:3.20".to_owned(),
            devcontainer_id: "workspace-id".to_owned(),
            final_user: "root".to_owned(),
            entrypoints: Vec::new(),
            install_env: BTreeMap::new(),
            context_dir,
            features: vec![FeatureLayerBuildFeature {
                id: "ghcr.io/example/features/tool".to_owned(),
                source_dir: source,
                option_env: BTreeMap::new(),
                container_env: BTreeMap::from([("BAD-NAME".to_owned(), "value".to_owned())]),
            }],
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Feature environment variable name is not supported"),
            "{error:#}"
        );
        assert!(error.to_string().contains("BAD-NAME"), "{error:#}");
        assert!(
            error.to_string().contains("ghcr.io/example/features/tool"),
            "{error:#}"
        );
    }

    #[test]
    fn feature_layer_build_context_rejects_invalid_base_image() {
        let temp = tempdir("feature-layer-build-context-invalid-base");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            source.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        let context_dir = temp.path().join("context");

        let error = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
            base_image: "alpine:3.20\nRUN false".to_owned(),
            devcontainer_id: "workspace-id".to_owned(),
            final_user: "root".to_owned(),
            entrypoints: Vec::new(),
            install_env: BTreeMap::new(),
            context_dir,
            features: vec![FeatureLayerBuildFeature {
                id: "ghcr.io/example/features/tool".to_owned(),
                source_dir: source,
                option_env: BTreeMap::new(),
                container_env: BTreeMap::new(),
            }],
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Docker image name contains unsupported whitespace"),
            "{error:#}"
        );
    }

    #[test]
    fn dockerignore_glob_star_does_not_cross_path_separator() {
        let temp = tempdir("dockerignore-glob-star");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("foo/bar")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("foo/root.txt"), "excluded-root\n").unwrap();
        fs::write(context_dir.join("foo/bar/baz.txt"), "included-nested\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "foo/*.txt\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "foo/root.txt"));
        assert!(tar_contains_path(&tar, "foo/bar/baz.txt"));
    }

    #[test]
    fn dockerignore_double_star_slash_matches_root_files() {
        let temp = tempdir("dockerignore-double-star-root");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("config")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("secret.env"), "excluded-root-secret\n").unwrap();
        fs::write(
            context_dir.join("config/secret.env"),
            "excluded-nested-secret\n",
        )
        .unwrap();
        fs::write(context_dir.join("app.txt"), "included-content\n").unwrap();
        fs::write(context_dir.join(".dockerignore"), "**/*.env\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "secret.env"));
        assert!(!tar_contains_path(&tar, "config/secret.env"));
        assert!(tar_contains_path(&tar, "app.txt"));
        let text = String::from_utf8_lossy(&tar);
        assert!(!text.contains("excluded-root-secret"));
        assert!(!text.contains("excluded-nested-secret"));
        assert!(text.contains("included-content"));
    }

    #[test]
    fn dockerignore_trailing_slash_matches_like_docker() {
        let temp = tempdir("dockerignore-trailing-slash");
        let root = temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(context_dir.join("node_modules")).unwrap();
        fs::create_dir_all(context_dir.join("app/node_modules")).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(context_dir.join("node_modules/pkg.json"), "excluded-root\n").unwrap();
        fs::write(
            context_dir.join("app/node_modules/pkg.json"),
            "included-nested\n",
        )
        .unwrap();
        fs::write(context_dir.join(".dockerignore"), "node_modules/\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(!tar_contains_path(&tar, "node_modules/pkg.json"));
        assert!(tar_contains_path(&tar, "app/node_modules"));
        assert!(tar_contains_path(&tar, "app/node_modules/pkg.json"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_archived_without_following_targets() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir("symlink-context");
        let outside_temp = tempdir("symlink-outside");
        let root = temp.path();
        let outside = outside_temp.path();
        let devcontainer_file = root.join(".devcontainer/devcontainer.json");
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        fs::write(outside.join("outside.txt"), "outside-content\n").unwrap();
        unix_fs::symlink(outside.join("outside.txt"), context_dir.join("linked.txt")).unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context = resolve_build_context(&root, &devcontainer_file, &build).unwrap();

        let tar = create_build_context_tar(&context).unwrap();

        assert!(tar_contains_path(&tar, "linked.txt"));
        assert!(!String::from_utf8_lossy(&tar).contains("outside-content"));
    }

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }

    fn docker_build_input(options: DockerBuildOptions) -> DockerBuildInput {
        let temp = tempdir("options");
        let root = temp.path();
        let context_dir = root.join(".devcontainer");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(context_dir.join("Dockerfile"), "FROM alpine\n").unwrap();
        let build = LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: None,
            args: Default::default(),
            target: None,
            cache_from: Vec::new(),
        };
        let context =
            resolve_build_context(root, &root.join(".devcontainer/devcontainer.json"), &build)
                .unwrap();

        DockerBuildInput {
            image_tag: "decune/test:options".to_owned(),
            labels: HashMap::new(),
            context,
            options,
        }
    }
}

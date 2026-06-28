use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::{context::ResolvedBuildContext, dockerfile_user, shell_quote};
use crate::docker::image::validate_image_name;

pub(crate) const FEATURE_ENTRYPOINT_WRAPPER: &str =
    "/usr/local/share/decune/feature-entrypoint-wrapper.sh";
pub(crate) const FEATURE_ENTRYPOINT_SENTINEL: &str = "/run/decune/feature-entrypoints-complete";
pub(crate) const FEATURE_ENTRYPOINT_TOKEN: &str = "/run/decune/feature-entrypoints-token";
const FEATURE_ENTRYPOINTS_FILE: &str = "decune-feature-entrypoints";
const FEATURE_ENTRYPOINT_WRAPPER_FILE: &str = "decune-feature-entrypoint-wrapper.sh";
const FEATURE_ENTRYPOINTS_TARGET: &str = "/usr/local/share/decune/feature-entrypoints";

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
        make_executable(&feature_dir.join("install.sh")).with_context(|| {
            format!(
                "Failed to make staged Feature install script executable: {}",
                feature_dir.join("install.sh").display()
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

fn feature_layer_dockerfile(input: &FeatureLayerBuildInput) -> Result<String> {
    validate_image_name(&input.base_image)?;
    let final_user = dockerfile_user(&input.final_user)?;
    let mut dockerfile = format!(
        "FROM {}\nUSER root\nRUN mkdir -p /usr/local/share/decune\nCOPY {FEATURE_ENTRYPOINT_WRAPPER_FILE} {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY {FEATURE_ENTRYPOINTS_FILE} {FEATURE_ENTRYPOINTS_TARGET}\nRUN chmod +x {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY . /tmp/decune-features/\n",
        input.base_image
    );
    for (index, feature) in input.features.iter().enumerate() {
        dockerfile.push_str(&feature_container_env_dockerfile(feature)?);
        let name = feature_context_name(index, &feature.id);
        writeln!(
            dockerfile,
            "RUN /bin/sh /tmp/decune-features/install-features.sh install {name}"
        )?;
    }
    dockerfile.push_str("RUN /bin/sh /tmp/decune-features/install-features.sh finish\n");
    writeln!(dockerfile, "USER {final_user}")?;
    Ok(dockerfile)
}

fn feature_layer_install_script(input: &FeatureLayerBuildInput) -> Result<String> {
    let mut script = String::new();
    script.push_str("set -eu\n");
    script.push_str(FEATURE_INSTALL_SCRIPT_FUNCTIONS);
    script.push_str(
        "DECUNE_WRAPPER_PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'\n",
    );
    write_feature_install_script_users(&mut script, input)?;
    script.push_str(FEATURE_INSTALL_SCRIPT_DISPATCH);
    Ok(script)
}

const FEATURE_INSTALL_SCRIPT_FUNCTIONS: &str = r#"decune_feature_user_home() {
    user="${1:-}"
    user="${user%%:*}"
    if [ -z "$user" ]; then
        return 0
    fi
    if PATH="$DECUNE_WRAPPER_PATH" command -v getent >/dev/null 2>&1; then
        record="$(PATH="$DECUNE_WRAPPER_PATH" getent passwd "$user" || true)"
        if [ -n "$record" ]; then
            old_ifs=$IFS
            IFS=:
            set -- $record
            IFS=$old_ifs
            printf '%s\n' "${6:-}"
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
decune_fix_feature_ownership() {
    fix_user="${1:-}"
    fix_user="${fix_user%%:*}"
    if [ -z "$fix_user" ] || [ "$fix_user" = "root" ] || [ "$fix_user" = "0" ]; then
        return 0
    fi
    fix_home="$(decune_feature_user_home "$fix_user")"
    if [ -z "$fix_home" ] || [ ! -d "$fix_home" ]; then
        return 0
    fi
    PATH="$DECUNE_WRAPPER_PATH" chown -R "$fix_user:" "$fix_home"
}
decune_install_feature() {
    feature_name="${1:-}"
    if [ -z "$feature_name" ]; then
        echo "decune Feature install target is missing" >&2
        exit 2
    fi
    (
    set -a
    . /tmp/decune-features/"$feature_name"/devcontainer-features.env
    _CONTAINER_USER_HOME="$(decune_feature_user_home "${_CONTAINER_USER:-}")"
    _REMOTE_USER_HOME="$(decune_feature_user_home "${_REMOTE_USER:-}")"
    export _CONTAINER_USER_HOME _REMOTE_USER_HOME
    set +a
    PATH="$DECUNE_WRAPPER_PATH" chmod +x /tmp/decune-features/"$feature_name"/install.sh
    cd /tmp/decune-features/"$feature_name"
    ./install.sh
    )
}
"#;

const FEATURE_INSTALL_SCRIPT_DISPATCH: &str = r#"case "${1:-}" in
    install)
        decune_install_feature "${2:-}"
        ;;
    finish)
        if [ -n "$DECUNE_REMOTE_USER" ]; then
            decune_fix_feature_ownership "$DECUNE_REMOTE_USER"
        fi
        if [ -n "$DECUNE_CONTAINER_USER" ]; then
            decune_fix_feature_ownership "$DECUNE_CONTAINER_USER"
        fi
        PATH="$DECUNE_WRAPPER_PATH" rm -rf /tmp/decune-features
        ;;
    *)
        echo "unsupported decune Feature install command: ${1:-}" >&2
        exit 2
        ;;
esac
"#;

fn write_feature_install_script_users(
    script: &mut String,
    input: &FeatureLayerBuildInput,
) -> Result<()> {
    let remote_user = input
        .install_env
        .get("_REMOTE_USER")
        .map_or("", String::as_str);
    let container_user = input
        .install_env
        .get("_CONTAINER_USER")
        .map_or("", String::as_str);
    if remote_user.is_empty() {
        script.push_str("DECUNE_REMOTE_USER=''\n");
    } else {
        writeln!(script, "DECUNE_REMOTE_USER={}", shell_quote(remote_user))?;
    }
    if !container_user.is_empty() && container_user != remote_user {
        writeln!(
            script,
            "DECUNE_CONTAINER_USER={}",
            shell_quote(container_user)
        )?;
    } else {
        script.push_str("DECUNE_CONTAINER_USER=''\n");
    }
    Ok(())
}

fn feature_container_env_dockerfile(feature: &FeatureLayerBuildFeature) -> Result<String> {
    let mut output = String::new();
    for (key, value) in &feature.container_env {
        validate_feature_env_key(&feature.id, key)?;
        writeln!(
            output,
            "ENV {key}=\"{}\"",
            dockerfile_env_value(&feature.id, key, value)?
        )?;
    }

    Ok(output)
}

fn dockerfile_env_value(feature_id: &str, key: &str, value: &str) -> Result<String> {
    if value
        .chars()
        .any(|character| character == '\0' || character == '\n' || character == '\r')
    {
        bail!(
            "Feature containerEnv value contains unsupported control characters for {feature_id}.{key}"
        );
    }

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
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

const fn feature_entrypoint_wrapper() -> &'static str {
    r#"#!/bin/sh
set -eu
feature_startup_id() {
    stat_line=$(cat /proc/1/stat 2>/dev/null || true)
    stat_tail=${stat_line#*) }
    set -- $stat_tail
    printf '%s' "${20:-unknown}"
}
sentinel=/run/decune/feature-entrypoints-complete
token_file=/run/decune/feature-entrypoints-token
sentinel_startup_id=$(feature_startup_id)
if [ ! -r "$token_file" ]; then
    echo "decune Feature entrypoint token is unavailable: $token_file" >&2
    exit 1
fi
sentinel_token=$(cat "$token_file")
: > "$sentinel"
if [ -f /usr/local/share/decune/feature-entrypoints ]; then
    while IFS= read -r entrypoint; do
        if [ -n "$entrypoint" ]; then
            /bin/sh -c "$entrypoint"
        fi
    done </usr/local/share/decune/feature-entrypoints
fi
printf '%s:%s\n' "$sentinel_startup_id" "$sentinel_token" > "$sentinel"
if [ "$#" -eq 0 ]; then
    trap 'exit 0' TERM
    while sleep 1 & wait $!; do :; done
fi
exec "$@"
"#
}

fn feature_env_file(
    input: &FeatureLayerBuildInput,
    feature: &FeatureLayerBuildFeature,
) -> Result<String> {
    let mut env = BTreeMap::new();
    env.extend(feature.option_env.clone());
    env.extend(input.install_env.clone());

    let mut output = String::new();
    for (key, value) in env {
        validate_feature_env_key(&feature.id, &key)?;
        writeln!(output, "{key}={}", shell_quote(&value))?;
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
    entries.sort_by_key(std::fs::DirEntry::path);

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

fn make_executable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect file permissions: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !metadata.is_file() {
        bail!(
            "Failed to make staged Feature install script executable: not a regular file: {}",
            path.display()
        );
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("Failed to update file permissions: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    use tempfile::TempDir;

    use super::{
        FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_TOKEN, FEATURE_ENTRYPOINT_WRAPPER,
        FEATURE_ENTRYPOINT_WRAPPER_FILE, FEATURE_ENTRYPOINTS_FILE, FeatureLayerBuildFeature,
        FeatureLayerBuildInput, ResolvedBuildContext, prepare_feature_layer_build_context,
    };
    use crate::docker::build::tar::{create_build_context_tar, tar_contains_path};

    const STAGED_TOOL_FEATURE_DIR: &str = "000-ghcr-io-example-features-tool";
    const EXPECTED_TOOL_ENV_FILE: &str = "VERSION='1.2'\"'\"'$(echo unsafe)'\n_CONTAINER_USER='root'\n_CONTAINER_USER_HOME='/root'\n_REMOTE_USER='vscode'\n_REMOTE_USER_HOME='/home/vscode'\n";

    #[test]
    fn feature_layer_build_context_stages_features_env_and_cleanup_dockerfile() {
        let temp = tempdir("feature-layer-build-context");
        let context = prepare_feature_layer_test_context(&temp);

        let tar = create_build_context_tar(&context).unwrap();
        let dockerfile = fs::read_to_string(&context.dockerfile_path).unwrap();
        let install_script =
            fs::read_to_string(context.context_dir.join("install-features.sh")).unwrap();
        let entrypoints =
            fs::read_to_string(context.context_dir.join(FEATURE_ENTRYPOINTS_FILE)).unwrap();
        let wrapper =
            fs::read_to_string(context.context_dir.join(FEATURE_ENTRYPOINT_WRAPPER_FILE)).unwrap();
        let staged_install = context
            .context_dir
            .join(STAGED_TOOL_FEATURE_DIR)
            .join("install.sh");

        assert_feature_layer_tar_entries(&tar);
        assert_feature_layer_dockerfile(&dockerfile);
        assert_feature_entrypoint_wrapper(&wrapper);
        assert_feature_install_script(&install_script, &staged_install);
        assert_eq!(entrypoints, "touch /tmp/feature-workspace-id\n");
        assert_feature_env_file(&context);
    }

    fn prepare_feature_layer_test_context(temp: &TempDir) -> ResolvedBuildContext {
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            source.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();

        prepare_feature_layer_build_context(&FeatureLayerBuildInput {
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
            context_dir: temp.path().join("context"),
            features: vec![FeatureLayerBuildFeature {
                id: "ghcr.io/example/features/tool".to_owned(),
                source_dir: source,
                option_env: BTreeMap::from([(
                    "VERSION".to_owned(),
                    "1.2'$(echo unsafe)".to_owned(),
                )]),
                container_env: BTreeMap::from([
                    ("PATH".to_owned(), "/opt/tool/bin:${PATH}".to_owned()),
                    (
                        "TOOL_FLAGS".to_owned(),
                        r#"quote" slash\ dollar$"#.to_owned(),
                    ),
                ]),
            }],
        })
        .unwrap()
    }

    fn assert_feature_layer_tar_entries(tar: &[u8]) {
        assert!(tar_contains_path(
            tar,
            &format!("{STAGED_TOOL_FEATURE_DIR}/install.sh")
        ));
        assert!(tar_contains_path(
            tar,
            &format!("{STAGED_TOOL_FEATURE_DIR}/devcontainer-features.env")
        ));
    }

    fn assert_feature_layer_dockerfile(dockerfile: &str) {
        assert!(dockerfile.contains("FROM alpine:3.20"));
        assert!(dockerfile.contains("ENV PATH=\"/opt/tool/bin:${PATH}\""));
        assert!(dockerfile.contains("ENV TOOL_FLAGS=\"quote\\\" slash\\\\ dollar$\""));
        assert!(dockerfile.contains(
            "RUN /bin/sh /tmp/decune-features/install-features.sh install 000-ghcr-io-example-features-tool"
        ));
        assert!(dockerfile.contains("RUN /bin/sh /tmp/decune-features/install-features.sh finish"));
        assert!(dockerfile.contains(FEATURE_ENTRYPOINT_WRAPPER));
        assert!(dockerfile.contains("USER vscode"));
    }

    fn assert_feature_entrypoint_wrapper(wrapper: &str) {
        assert!(wrapper.contains(FEATURE_ENTRYPOINT_SENTINEL));
        assert!(wrapper.contains(FEATURE_ENTRYPOINT_TOKEN));
        assert!(wrapper.contains("sentinel_startup_id=$(feature_startup_id)"));
        assert!(wrapper.contains("if [ ! -r \"$token_file\" ]; then"));
        assert!(wrapper.contains("decune Feature entrypoint token is unavailable"));
        assert!(wrapper.contains("sentinel_token=$(cat \"$token_file\")"));
        assert!(wrapper.contains(": > \"$sentinel\""));
        assert!(wrapper.contains(
            "printf '%s:%s\\n' \"$sentinel_startup_id\" \"$sentinel_token\" > \"$sentinel\""
        ));
        assert!(!wrapper.contains("rm -f \"$sentinel\""));
        assert!(!wrapper.contains("mkdir -p /run/decune"));
    }

    fn assert_feature_install_script(install_script: &str, staged_install: &Path) {
        assert!(install_script.contains("./install.sh"));
        assert!(!install_script.contains("/bin/sh ./install.sh"));
        assert!(install_script.contains("rm -rf /tmp/decune-features"));
        assert_ne!(
            fs::metadata(staged_install).unwrap().permissions().mode() & 0o111,
            0
        );
        assert!(install_script.contains("decune_fix_feature_ownership()"));
        assert!(install_script.contains("DECUNE_REMOTE_USER='vscode'"));
        assert!(install_script.contains("DECUNE_CONTAINER_USER='root'"));
        assert!(install_script.contains("PATH=\"$DECUNE_WRAPPER_PATH\" chmod +x"));
        let install_pos = install_script.find("./install.sh").unwrap();
        let fix_pos = install_script
            .find("decune_fix_feature_ownership \"$DECUNE_REMOTE_USER\"")
            .unwrap();
        let cleanup_pos = install_script.find("rm -rf /tmp/decune-features").unwrap();
        assert!(install_pos < fix_pos);
        assert!(fix_pos < cleanup_pos);
    }

    fn assert_feature_env_file(context: &ResolvedBuildContext) {
        let env_file = fs::read_to_string(
            context
                .context_dir
                .join(STAGED_TOOL_FEATURE_DIR)
                .join("devcontainer-features.env"),
        )
        .unwrap();
        assert_eq!(env_file, EXPECTED_TOOL_ENV_FILE);
        assert!(!env_file.contains("PATH="));
        assert!(!env_file.contains("TOOL_FLAGS="));
    }

    #[test]
    fn feature_layer_build_context_does_not_chmod_symlinked_install_script_target() {
        let temp = tempdir("feature-layer-build-context-symlink-install");
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let external_install = temp.path().join("external-install.sh");
        fs::write(&external_install, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&external_install, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&external_install, source.join("install.sh")).unwrap();
        fs::write(
            source.join("devcontainer-feature.json"),
            r#"{"id":"tool","version":"1.0.0","name":"Tool"}"#,
        )
        .unwrap();
        let context_dir = temp.path().join("context");
        let context = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
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
                container_env: BTreeMap::new(),
            }],
        })
        .unwrap();

        let external_mode = fs::metadata(&external_install)
            .unwrap()
            .permissions()
            .mode();
        let staged_install = context
            .context_dir
            .join("000-ghcr-io-example-features-tool/install.sh");

        assert_eq!(external_mode & 0o777, 0o644);
        assert!(
            fs::symlink_metadata(staged_install)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
    fn feature_layer_build_context_rejects_multiline_container_env_value() {
        let temp = tempdir("feature-layer-build-context-multiline-env");
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
                container_env: BTreeMap::from([("TOOL_FLAGS".to_owned(), "one\ntwo".to_owned())]),
            }],
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Feature containerEnv value contains unsupported control characters"),
            "{error:#}"
        );
        assert!(error.to_string().contains("TOOL_FLAGS"), "{error:#}");
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

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}

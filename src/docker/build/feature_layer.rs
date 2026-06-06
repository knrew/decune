use std::{
    collections::BTreeMap,
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
    Ok(format!(
        "FROM {}\nUSER root\nRUN mkdir -p /usr/local/share/decune\nCOPY {FEATURE_ENTRYPOINT_WRAPPER_FILE} {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY {FEATURE_ENTRYPOINTS_FILE} {FEATURE_ENTRYPOINTS_TARGET}\nRUN chmod +x {FEATURE_ENTRYPOINT_WRAPPER}\nCOPY . /tmp/decune-features/\nRUN /bin/sh /tmp/decune-features/install-features.sh\nUSER {final_user}\n",
        input.base_image
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
        output.push_str(&format!("{key}={}\n", shell_quote(&value)));
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

fn make_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("Failed to inspect file permissions: {}", path.display()))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("Failed to update file permissions: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use super::{
        FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_WRAPPER, FEATURE_ENTRYPOINT_WRAPPER_FILE,
        FEATURE_ENTRYPOINTS_FILE, FeatureLayerBuildFeature, FeatureLayerBuildInput,
        prepare_feature_layer_build_context,
    };
    use crate::docker::build::tar::{create_build_context_tar, tar_contains_path};

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
        let staged_install = context
            .context_dir
            .join("000-ghcr-io-example-features-tool/install.sh");

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
        assert_ne!(
            fs::metadata(staged_install).unwrap().permissions().mode() & 0o111,
            0
        );
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

    fn tempdir(name: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("decune-docker-build-{name}-"))
            .tempdir()
            .unwrap()
    }
}

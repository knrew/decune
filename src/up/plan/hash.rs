use anyhow::{Context, Result};

use crate::{
    config::{ConfigHashInput, FeatureLockHashEntry, resolved::ResolvedConfig},
    devcontainer::features::{
        FeatureRef, parse_feature_ref_from_devcontainer_dir, read_feature_lock_file,
        resolve_locked_feature_ref,
    },
    workspace::Workspace,
};

const FEATURE_ENTRYPOINT_SHIM_HASH_VERSION: &str = "3";
const FEATURE_LAYER_HASH_VERSION: &str = "2";

pub(in crate::up) fn feature_lock_hash_inputs(
    workspace: &Workspace,
    devcontainer_file: &std::path::Path,
    config: &ResolvedConfig,
    update_features: bool,
) -> Result<Vec<FeatureLockHashEntry>> {
    if config.features.is_empty() {
        return Ok(Vec::new());
    }

    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Failed to resolve devcontainer directory for {}",
            devcontainer_file.display()
        )
    })?;
    let references = config
        .features
        .iter()
        .map(|feature| {
            parse_feature_ref_from_devcontainer_dir(&feature.id, devcontainer_dir)
                .with_context(|| format!("Failed to parse Feature ref: {}", feature.id))
        })
        .collect::<Result<Vec<_>>>()?;

    if update_features {
        return Ok(Vec::new());
    }

    let lock_path = workspace.root().join(".decune").join("features.lock.toml");
    let lock = read_feature_lock_file(&lock_path)?;
    let mut entries = Vec::new();

    for reference in references {
        let _resolved = resolve_locked_feature_ref(&reference, &lock, false);
        let canonical_id = reference.canonical_id().to_owned();

        if let FeatureRef::Oci(reference) = reference
            && let Some(digest) = lock.digest_for_reference(&reference)
        {
            entries.push(FeatureLockHashEntry {
                feature_id: canonical_id,
                digest: digest.to_owned(),
            });
        }
    }

    Ok(entries)
}

pub(in crate::up) fn add_internal_hash_versions(
    input: &mut ConfigHashInput<'_>,
    config: &ResolvedConfig,
) {
    if !config.features.is_empty() {
        input.internal_versions.insert(
            "feature_layer".to_owned(),
            FEATURE_LAYER_HASH_VERSION.to_owned(),
        );
    }
    if !config.devcontainer.entrypoints.is_empty() {
        input.internal_versions.insert(
            "feature_entrypoint_shim".to_owned(),
            FEATURE_ENTRYPOINT_SHIM_HASH_VERSION.to_owned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        config::ConfigLayer,
        docker::mounts::{MountBindOptions, MountBindPropagation, MountVolumeOptions},
        up::{
            plan::{
                build_up_plan, build_up_plan_with_image_metadata,
                build_up_plan_with_update_features,
            },
            test_support::{
                config_hash_for_mount, test_mount, test_volume_mount, test_workspace,
                write_devcontainer,
            },
        },
    };

    #[test]
    fn build_up_plan_includes_feature_lock_digest_in_config_hash() {
        let workspace = test_workspace("feature-lock-hash");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/example/features/tool:1": {}
          }
        }
        "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
    }

    #[test]
    fn build_up_plan_ignores_feature_lock_digest_when_features_are_updated() {
        let workspace = test_workspace("feature-lock-update-hash");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/example/features/tool:1": {}
          }
        }
        "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let updated =
            build_up_plan_with_update_features(&workspace, None, ConfigLayer::default(), true)
                .unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
        assert_eq!(
            baseline.resources.config_hash,
            updated.resources.config_hash
        );
    }

    #[test]
    fn build_up_plan_rejects_invalid_feature_ref_with_ref_in_error() {
        let workspace = test_workspace("invalid-feature-ref");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/features": {}
          }
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains("ghcr.io/features"), "{error:#}");
    }

    #[test]
    fn build_up_plan_merges_image_metadata_and_includes_it_in_config_hash() {
        let workspace = test_workspace("image-metadata-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_env: [("FROM_IMAGE".to_owned(), "1".to_owned())].into(),
                remote_user: Some("image-user".to_owned()),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };
        let changed_image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_env: [("FROM_IMAGE".to_owned(), "2".to_owned())].into(),
                remote_user: Some("image-user".to_owned()),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };

        let plan = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![image_layer],
        )
        .unwrap();
        let changed = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![changed_image_layer],
        )
        .unwrap();

        assert_eq!(
            plan.config.devcontainer.remote_user.as_deref(),
            Some("image-user")
        );
        assert_eq!(
            plan.config
                .devcontainer
                .remote_env
                .get("FROM_IMAGE")
                .map(String::as_str),
            Some("1")
        );
        assert_ne!(plan.resources.config_hash, changed.resources.config_hash);
    }

    #[cfg(unix)]
    #[test]
    fn build_up_plan_hash_changes_when_resolved_mount_source_changes() {
        let workspace = test_workspace("mount-source-hash-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join("first-cache")).unwrap();
        fs::create_dir_all(workspace.root().join("second-cache")).unwrap();
        let link = workspace.root().join("host-cache");
        std::os::unix::fs::symlink(workspace.root().join("first-cache"), &link).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/cache"
type = "bind"
resolve_symlink = true
"#,
        )
        .unwrap();

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(workspace.root().join("second-cache"), &link).unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.mounts[1].source, second.mounts[1].source);
        assert_ne!(first.resources.config_hash, second.resources.config_hash);
    }

    #[test]
    fn config_hash_changes_when_resolved_mount_options_change() {
        let mut cached = test_mount();
        cached.consistency = Some("cached".to_owned());
        let mut delegated = test_mount();
        delegated.consistency = Some("delegated".to_owned());
        assert_ne!(
            config_hash_for_mount(cached),
            config_hash_for_mount(delegated)
        );

        let mut rshared = test_mount();
        rshared.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RShared),
            ..MountBindOptions::default()
        });
        let mut rslave = test_mount();
        rslave.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RSlave),
            ..MountBindOptions::default()
        });
        assert_ne!(
            config_hash_for_mount(rshared),
            config_hash_for_mount(rslave)
        );

        let mut deps = test_volume_mount();
        deps.volume_options = Some(MountVolumeOptions {
            subpath: Some("deps".to_owned()),
            ..MountVolumeOptions::default()
        });
        let mut cache = test_volume_mount();
        cache.volume_options = Some(MountVolumeOptions {
            subpath: Some("cache".to_owned()),
            ..MountVolumeOptions::default()
        });
        assert_ne!(config_hash_for_mount(deps), config_hash_for_mount(cache));
    }
}

// Verifies UID/GID sync decisions that do not require Docker.

use std::collections::BTreeMap;

use super::{effective_user_input_from_plan, uid_gid_sync_base_image, uid_gid_sync_warning};
use crate::{
    config::{
        ConfigLayer, ConfigMergeInput,
        layer::LayerDevcontainerMetadata,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
    },
    docker::{
        build::DockerBuildOptions,
        resource::DockerResources,
        user::{EffectiveUsers, HostPlatform, UidGidSyncNoopReason, UidGidSyncPlan},
    },
    up::{
        test_support::{
            sync_plan, test_resources, test_up_plan_with_config, test_up_plan_with_image_source,
        },
        types::UpPlan,
    },
};

#[test]
fn uid_gid_sync_warning_reports_only_explicit_true_on_non_linux() {
    let plan = UidGidSyncPlan::Noop {
        reason: UidGidSyncNoopReason::NonLinuxHost,
    };
    let default_layers = ConfigMergeInput::default();
    let explicit_true = layers_with_update_remote_user_uid(true);
    let explicit_false = layers_with_update_remote_user_uid(false);

    assert_eq!(
        uid_gid_sync_warning(&default_layers, &plan, true, HostPlatform::NonLinux),
        None
    );
    assert_eq!(
        uid_gid_sync_warning(&explicit_true, &plan, true, HostPlatform::NonLinux),
        Some(
            "UID/GID sync is only supported on Linux hosts; skipping updateRemoteUserUID"
                .to_owned()
        )
    );
    assert_eq!(
        uid_gid_sync_warning(&explicit_false, &plan, false, HostPlatform::NonLinux),
        None
    );
}

#[test]
fn effective_user_input_from_plan_uses_expanded_user_values() {
    let devcontainer_layer = ConfigLayer {
        devcontainer: Some(LayerDevcontainerMetadata {
            remote_user: Some("${localEnv:DECUNE_TEST_REMOTE_USER}".to_owned()),
            container_user: Some("${localEnv:DECUNE_TEST_CONTAINER_USER}".to_owned()),
            ..LayerDevcontainerMetadata::default()
        }),
        ..ConfigLayer::default()
    };
    let mut config = ResolvedConfig::default();
    config.devcontainer.remote_user = Some("remoteuser".to_owned());
    config.devcontainer.container_user = Some("containeruser".to_owned());
    let plan = test_plan(
        config,
        ConfigMergeInput {
            devcontainer: Some(devcontainer_layer),
            ..ConfigMergeInput::default()
        },
    );

    let input = effective_user_input_from_plan(&plan);

    assert_eq!(input.devcontainer_remote, Some("remoteuser"));
    assert_eq!(input.devcontainer_container, Some("containeruser"));
    assert_eq!(input.image_metadata_remote, None);
    assert_eq!(input.image_metadata_container, None);
}

#[test]
fn effective_user_input_from_plan_keeps_metadata_user_origin() {
    let feature_layer = ConfigLayer {
        devcontainer: Some(LayerDevcontainerMetadata {
            remote_user: Some("${localEnv:DECUNE_TEST_FEATURE_REMOTE_USER}".to_owned()),
            container_user: Some("${localEnv:DECUNE_TEST_FEATURE_CONTAINER_USER}".to_owned()),
            ..LayerDevcontainerMetadata::default()
        }),
        ..ConfigLayer::default()
    };
    let mut config = ResolvedConfig::default();
    config.devcontainer.remote_user = Some("feature-remote".to_owned());
    config.devcontainer.container_user = Some("feature-container".to_owned());
    let plan = test_plan(
        config,
        ConfigMergeInput {
            feature_metadata: vec![feature_layer],
            ..ConfigMergeInput::default()
        },
    );

    let input = effective_user_input_from_plan(&plan);

    assert_eq!(input.devcontainer_remote, None);
    assert_eq!(input.devcontainer_container, None);
    assert_eq!(input.image_metadata_remote, Some("feature-remote"));
    assert_eq!(input.image_metadata_container, Some("feature-container"));
}

#[test]
fn uid_gid_sync_warning_reports_numeric_user_without_passwd_noop() {
    let warning = uid_gid_sync_warning(
        &ConfigMergeInput::default(),
        &UidGidSyncPlan::Noop {
            reason: UidGidSyncNoopReason::NumericUserWithoutPasswd,
        },
        true,
        HostPlatform::Linux,
    );

    assert_eq!(
        warning.as_deref(),
        Some("UID/GID sync is skipped because the configured numeric user has no passwd entry")
    );
}

#[test]
fn dockerfile_uid_gid_sync_base_uses_resolved_base_image_without_workspace_layer() {
    let mut config = ResolvedConfig::default();
    config.devcontainer.source = Some(ResolvedDevcontainerSource::Dockerfile(
        crate::config::layer::LayerDevcontainerBuild {
            dockerfile: "Dockerfile".to_owned(),
            context: Some(".".to_owned()),
            args: BTreeMap::new(),
            options: Vec::new(),
            target: None,
            cache_from: Vec::new(),
        },
    ));
    let mut plan = test_up_plan_with_config(config);
    plan.image = "decune/test:final-sync-hash".to_owned();
    plan.base_image = "decune/test:pre-sync-hash".to_owned();
    plan.resources.image_tag = plan.image.clone();
    plan.resources.config_hash = "final-sync-hash".to_owned();
    plan.pre_uid_gid_sync_resources = Some(test_resources("pre-sync-hash"));
    plan.uid_gid_sync_plan = sync_plan();

    assert_eq!(uid_gid_sync_base_image(&plan), "decune/test:pre-sync-hash");
}

#[test]
fn image_uid_gid_sync_base_uses_original_image_without_workspace_layer() {
    let mut plan = test_up_plan_with_image_source("alpine:3.20");
    plan.image = "decune/test:final-sync-hash".to_owned();
    plan.resources.image_tag = plan.image.clone();
    plan.resources.config_hash = "final-sync-hash".to_owned();
    plan.pre_uid_gid_sync_resources = Some(test_resources("pre-sync-hash"));
    plan.uid_gid_sync_plan = sync_plan();

    assert_eq!(uid_gid_sync_base_image(&plan), "alpine:3.20");
}

fn layers_with_update_remote_user_uid(update_remote_user_uid: bool) -> ConfigMergeInput {
    ConfigMergeInput {
        devcontainer: Some(ConfigLayer {
            devcontainer: Some(LayerDevcontainerMetadata {
                update_remote_user_uid: Some(update_remote_user_uid),
                ..LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        }),
        ..ConfigMergeInput::default()
    }
}

fn test_plan(config: ResolvedConfig, config_layers: ConfigMergeInput) -> UpPlan {
    UpPlan {
        image: "alpine:3.20".to_owned(),
        base_image: "alpine:3.20".to_owned(),
        build_context: None,
        build_options: DockerBuildOptions::default(),
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources: DockerResources {
            container_name: "decune-test".to_owned(),
            image_tag: "decune/test:stable-hash".to_owned(),
            workspace_volume_name: "decune-test-workspace".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "stable-hash".to_owned(),
        },
        pre_uid_gid_sync_resources: None,
        compose_project: None,
        config_layers,
        config,
        sensitive_container_env: crate::config::variables::SensitiveEnvMap::default(),
        sensitive_build_args: crate::config::variables::SensitiveEnvMap::default(),
        compose_interpolation_env: BTreeMap::default(),
        compose_interpolation_redactions: Vec::new(),
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: "/workspaces/project".to_owned(),
        mounts: Vec::new(),
        dotfile_skeletons: Vec::new(),
        forward_ports: Vec::new(),
        ignored_detached_forwarding: false,
    }
}

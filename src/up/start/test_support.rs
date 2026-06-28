use std::{collections::BTreeMap, path::PathBuf};

use crate::{
    config::{ConfigLayer, ConfigMergeInput, resolved::ResolvedConfig},
    docker::{
        build::DockerBuildOptions,
        mounts::DockerMountSpec,
        resource::DockerResources,
        user::{
            EffectiveUsers, HostUserIds, ResolvedUserIds, UidGidSyncPlan, UidGidSyncTarget,
            UidGidSyncTargetKind,
        },
    },
    up::{
        UpContainerSummary, UpMountSummary, UpOptions, UpPlan,
        existing::CredentialRuntimeMountPolicy,
    },
};

pub(super) fn generated_override_test_plan(mounts: Vec<DockerMountSpec>) -> UpPlan {
    let mut config = ResolvedConfig::default();
    config.devcontainer.override_command = false;
    let resources = DockerResources {
        container_name: "unused".to_owned(),
        image_tag: "decune/test:hash".to_owned(),
        workspace_volume_name: "unused-volume".to_owned(),
        labels: BTreeMap::new(),
        config_hash: "hash".to_owned(),
    };

    UpPlan {
        image: "decune/test:hash".to_owned(),
        base_image: "alpine:3.20".to_owned(),
        build_context: None,
        build_options: DockerBuildOptions::default(),
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources,
        pre_uid_gid_sync_resources: None,
        compose_project: None,
        config_layers: ConfigMergeInput::default(),
        config,
        sensitive_container_env: crate::config::variables::SensitiveEnvMap::default(),
        sensitive_build_args: crate::config::variables::SensitiveEnvMap::default(),
        compose_interpolation_env: BTreeMap::default(),
        compose_interpolation_redactions: Vec::new(),
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: "/workspace".to_owned(),
        mounts,
        dotfile_skeletons: Vec::new(),
        forward_ports: Vec::new(),
        ignored_detached_forwarding: false,
    }
}

pub(super) fn up_options_for_fast_path() -> UpOptions {
    UpOptions {
        workspace: PathBuf::from("/workspace"),
        config_path: None,
        cli_layer: ConfigLayer::default(),
        config: crate::up::UpConfigOptions::default(),
        build: crate::up::UpBuildOptions::default(),
        reuse: crate::up::UpReuseOptions::default(),
    }
}

pub(super) fn reusable_container(config_hash: &str) -> UpContainerSummary {
    UpContainerSummary {
        id: "container-id".to_owned(),
        name: "project-app-1".to_owned(),
        image_id: Some("sha256:image".to_owned()),
        config_hash: Some(config_hash.to_owned()),
        config_file: None,
        mounts: Some(Vec::new()),
        running: true,
    }
}

pub(super) fn mount_policy(required_mounts: &[UpMountSummary]) -> CredentialRuntimeMountPolicy {
    CredentialRuntimeMountPolicy::new(required_mounts.to_vec())
}

pub(super) fn sync_plan() -> UidGidSyncPlan {
    UidGidSyncPlan::Sync {
        target: UidGidSyncTarget {
            kind: UidGidSyncTargetKind::ContainerUser,
            user: "2001:2001".to_owned(),
            host: HostUserIds {
                uid: 1000,
                gid: 1000,
            },
        },
        container: ResolvedUserIds {
            name: "syncuser".to_owned(),
            uid: 2001,
            gid: 2001,
        },
    }
}

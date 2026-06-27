use crate::{
    config::{ConfigLayer, ConfigMergeInput, UidGidSyncHashInput, UidGidSyncHashState},
    docker::{
        resource::DockerResources,
        user::{
            EffectiveUserResolveInput, HostPlatform, UidGidSyncNoopReason, UidGidSyncPlan,
            UidGidSyncTargetKind, current_host_user_ids,
        },
    },
};

use super::UpPlan;

pub(super) fn effective_user_input_from_config_layers(
    config_layers: &ConfigMergeInput,
) -> EffectiveUserResolveInput<'_> {
    EffectiveUserResolveInput {
        devcontainer_remote_user: config_layers
            .devcontainer
            .as_ref()
            .and_then(layer_remote_user),
        devcontainer_container_user: config_layers
            .devcontainer
            .as_ref()
            .and_then(layer_container_user),
        image_metadata_remote_user: merged_metadata_remote_user(config_layers),
        image_metadata_container_user: merged_metadata_container_user(config_layers),
        image_config_user: None,
    }
}

pub(super) fn effective_user_input_from_plan(plan: &UpPlan) -> EffectiveUserResolveInput<'_> {
    let layer_input = effective_user_input_from_config_layers(&plan.config_layers);
    let devcontainer_remote_user = layer_input
        .devcontainer_remote_user
        .and(plan.config.devcontainer.remote_user.as_deref());
    let image_metadata_remote_user = if devcontainer_remote_user.is_some() {
        None
    } else {
        layer_input
            .image_metadata_remote_user
            .and(plan.config.devcontainer.remote_user.as_deref())
    };
    let devcontainer_container_user = layer_input
        .devcontainer_container_user
        .and(plan.config.devcontainer.container_user.as_deref());
    let image_metadata_container_user = if devcontainer_container_user.is_some() {
        None
    } else {
        layer_input
            .image_metadata_container_user
            .and(plan.config.devcontainer.container_user.as_deref())
    };

    EffectiveUserResolveInput {
        devcontainer_remote_user,
        devcontainer_container_user,
        image_metadata_remote_user,
        image_metadata_container_user,
        image_config_user: None,
    }
}

pub(super) fn uid_gid_sync_hash_input(
    plan: &UidGidSyncPlan,
    update_remote_user_uid: bool,
    host_platform: HostPlatform,
) -> Option<UidGidSyncHashInput> {
    if !update_remote_user_uid || host_platform != HostPlatform::Linux {
        return None;
    }

    Some(match plan {
        UidGidSyncPlan::Sync { target, .. } => UidGidSyncHashInput {
            state: UidGidSyncHashState::Sync,
            host_uid: target.host.uid,
            host_gid: target.host.gid,
            target_kind: Some(uid_gid_sync_target_kind_name(target.kind).to_owned()),
            target_user: Some(target.user.clone()),
        },
        UidGidSyncPlan::Noop { reason } => UidGidSyncHashInput {
            state: UidGidSyncHashState::Noop(uid_gid_sync_noop_reason_name(*reason).to_owned()),
            host_uid: current_host_user_ids().uid,
            host_gid: current_host_user_ids().gid,
            target_kind: None,
            target_user: None,
        },
    })
}

pub(super) fn static_uid_gid_sync_hash_input(
    config_layers: &ConfigMergeInput,
    update_remote_user_uid: bool,
) -> Option<UidGidSyncHashInput> {
    if !update_remote_user_uid || HostPlatform::current() != HostPlatform::Linux {
        return None;
    }

    let input = effective_user_input_from_config_layers(config_layers);
    if input.devcontainer_remote_user.is_some()
        || input.image_metadata_remote_user.is_some()
        || input.devcontainer_container_user.is_some()
        || input.image_metadata_container_user.is_some()
    {
        return None;
    }

    let host = current_host_user_ids();
    Some(UidGidSyncHashInput {
        state: UidGidSyncHashState::Noop(
            uid_gid_sync_noop_reason_name(UidGidSyncNoopReason::NoExplicitUser).to_owned(),
        ),
        host_uid: host.uid,
        host_gid: host.gid,
        target_kind: None,
        target_user: None,
    })
}

fn uid_gid_sync_target_kind_name(kind: UidGidSyncTargetKind) -> &'static str {
    match kind {
        UidGidSyncTargetKind::RemoteUser => "remoteUser",
        UidGidSyncTargetKind::ContainerUser => "containerUser",
    }
}

fn uid_gid_sync_noop_reason_name(reason: UidGidSyncNoopReason) -> &'static str {
    match reason {
        UidGidSyncNoopReason::Disabled => "disabled",
        UidGidSyncNoopReason::NonLinuxHost => "nonLinuxHost",
        UidGidSyncNoopReason::NoExplicitUser => "noExplicitUser",
        UidGidSyncNoopReason::NumericUserWithoutPasswd => "numericUserWithoutPasswd",
        UidGidSyncNoopReason::Root => "root",
    }
}

pub(crate) fn uid_gid_sync_warning(
    config_layers: &ConfigMergeInput,
    plan: &UidGidSyncPlan,
    update_remote_user_uid: bool,
    host_platform: HostPlatform,
) -> Option<String> {
    if host_platform != HostPlatform::Linux {
        if explicit_update_remote_user_uid(config_layers) == Some(true) {
            return Some(
                "UID/GID sync is only supported on Linux hosts; skipping updateRemoteUserUID"
                    .to_owned(),
            );
        }

        return None;
    }

    if update_remote_user_uid
        && matches!(
            plan,
            UidGidSyncPlan::Noop {
                reason: UidGidSyncNoopReason::NumericUserWithoutPasswd
            }
        )
    {
        return Some(
            "UID/GID sync is skipped because the configured numeric user has no passwd entry"
                .to_owned(),
        );
    }

    None
}

fn explicit_update_remote_user_uid(config_layers: &ConfigMergeInput) -> Option<bool> {
    let mut explicit = None;
    for layer in config_layers
        .image_metadata
        .iter()
        .chain(config_layers.feature_metadata.iter())
        .chain(config_layers.global.iter())
        .chain(config_layers.devcontainer.iter())
        .chain(config_layers.project.iter())
        .chain(config_layers.cli.iter())
    {
        if let Some(update_remote_user_uid) = layer
            .devcontainer
            .as_ref()
            .and_then(|devcontainer| devcontainer.update_remote_user_uid)
        {
            explicit = Some(update_remote_user_uid);
        }
    }

    explicit
}

fn merged_metadata_remote_user(config_layers: &ConfigMergeInput) -> Option<&str> {
    config_layers
        .image_metadata
        .iter()
        .chain(config_layers.feature_metadata.iter())
        .filter_map(layer_remote_user)
        .next_back()
}

fn merged_metadata_container_user(config_layers: &ConfigMergeInput) -> Option<&str> {
    config_layers
        .image_metadata
        .iter()
        .chain(config_layers.feature_metadata.iter())
        .filter_map(layer_container_user)
        .next_back()
}

fn layer_remote_user(layer: &ConfigLayer) -> Option<&str> {
    layer
        .devcontainer
        .as_ref()
        .and_then(|devcontainer| devcontainer.remote_user.as_deref())
}

fn layer_container_user(layer: &ConfigLayer) -> Option<&str> {
    layer
        .devcontainer
        .as_ref()
        .and_then(|devcontainer| devcontainer.container_user.as_deref())
}

pub(crate) fn uid_gid_sync_base_image(plan: &UpPlan) -> String {
    if plan_requires_workspace_layer(plan) {
        return pre_uid_gid_sync_layer_resources(plan).image_tag.clone();
    }

    plan.base_image.clone()
}

pub(super) fn pre_uid_gid_sync_layer_resources(plan: &UpPlan) -> &DockerResources {
    plan.pre_uid_gid_sync_resources
        .as_ref()
        .unwrap_or(&plan.resources)
}

pub(super) fn plan_requires_uid_gid_sync_layer(plan: &UpPlan) -> bool {
    uid_gid_sync_plan_requires_layer(&plan.uid_gid_sync_plan)
}

pub(super) fn uid_gid_sync_plan_requires_layer(plan: &UidGidSyncPlan) -> bool {
    matches!(plan, UidGidSyncPlan::Sync { .. })
}

pub(super) fn effective_users_depend_on_image_config_user(plan: &UpPlan) -> bool {
    let input = effective_user_input_from_config_layers(&plan.config_layers);
    input.devcontainer_remote_user.is_none()
        && input.image_metadata_remote_user.is_none()
        && input.devcontainer_container_user.is_none()
        && input.image_metadata_container_user.is_none()
}

fn plan_requires_workspace_layer(plan: &UpPlan) -> bool {
    plan.feature_install.is_some()
        || !plan.config.features.is_empty()
        || !plan.config.devcontainer.entrypoints.is_empty()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::{effective_user_input_from_plan, uid_gid_sync_base_image, uid_gid_sync_warning};
    use crate::{
        config::{
            ConfigLayer, ConfigMergeInput,
            layer::LayerDevcontainerMetadata,
            resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        },
        docker::{
            build::DockerBuildOptions,
            client::DockerClient,
            container::remove_container,
            exec::{ExecCommandSpec, exec_capture},
            image::{PullPolicy, ensure_image, remove_image},
            resource::DockerResources,
            user::{
                EffectiveUsers, HostPlatform, UidGidSyncNoopReason, UidGidSyncPlan,
                current_host_user_ids,
            },
        },
        up::{
            UpOptions,
            plan::build_up_plan,
            run_detached_up,
            start::list_workspace_containers,
            test_support::{
                build_distinct_uid_gid_users_image, build_duplicate_matching_host_ids_image,
                build_duplicate_old_gid_image, build_missing_target_group_conflict_image,
                build_named_uid_numeric_gid_user_image, build_numeric_uid_gid_user_image,
                build_uid_gid_conflict_user_image, build_uid_gid_user_image, sync_plan,
                test_resources, test_up_plan_with_config, test_up_plan_with_image_source,
                test_workspace, write_devcontainer,
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

        assert_eq!(input.devcontainer_remote_user, Some("remoteuser"));
        assert_eq!(input.devcontainer_container_user, Some("containeruser"));
        assert_eq!(input.image_metadata_remote_user, None);
        assert_eq!(input.image_metadata_container_user, None);
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

        assert_eq!(input.devcontainer_remote_user, None);
        assert_eq!(input.devcontainer_container_user, None);
        assert_eq!(input.image_metadata_remote_user, Some("feature-remote"));
        assert_eq!(
            input.image_metadata_container_user,
            Some("feature-container")
        );
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
            sensitive_container_env: Default::default(),
            sensitive_build_args: Default::default(),
            compose_interpolation_env: Default::default(),
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
    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_remote_user_uid_gid_on_linux_host() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync");
            let image = format!("decune-test/uid-gid-sync-{}:latest", workspace.id());
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: Some("syncuser".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_container_user_uid_gid_when_remote_user_is_not_set() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-container-user");
            let image = format!(
                "decune-test/uid-gid-sync-container-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "syncuser",
                      "postCreateCommand": "id -u >/tmp/decune-container-user-ids; id -g >>/tmp/decune-container-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some("syncuser".to_owned())
                );
                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-container-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_syncs_remote_user_without_renumbering_distinct_container_user() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0
            || host.gid == 0
            || host.uid == 2001
            || host.gid == 2001
            || host.uid == 2002
            || host.gid == 2002
        {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-distinct-users");
            let image = format!(
                "decune-test/uid-gid-sync-distinct-users-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "containerUser": "containeruser",
                      "remoteUser": "remoteuser",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_distinct_uid_gid_users_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let remote_output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(remote_output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                let container_output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u containeruser; id -g containeruser".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(container_output.stdout)?, "2002\n2002\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_does_not_sync_remote_user_when_update_remote_user_uid_is_false() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || (host.uid == 2001 && host.gid == 2001) {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-disabled");
            let image = format!(
                "decune-test/uid-gid-sync-disabled-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser",
                      "updateRemoteUserUID": false,
                      "postCreateCommand": "id -u >/tmp/decune-disabled-user-ids; id -g >>/tmp/decune-disabled-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-disabled-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout)?, "2001\n2001\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_applies_uid_gid_sync_after_feature_layer() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-after-feature");
            let image = format!(
                "decune-test/uid-gid-sync-after-feature-{}:latest",
                workspace.id()
            );
            fs::create_dir_all(workspace.root().join(".devcontainer/features/order-tool"))
                .unwrap();
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "features": {{
                        "./features/order-tool": {{}}
                      }},
                      "remoteUser": "syncuser",
                      "postCreateCommand": "test \"$(cat /usr/local/share/decune-feature-syncuser-uid)\" = 2001 && test \"$(id -u)\" = {host_uid} && test \"$(id -g)\" = {host_gid}"
                    }}
                    "#,
                    host_uid = host.uid,
                    host_gid = host.gid,
                ),
            );
            fs::write(
                workspace
                    .root()
                    .join(".devcontainer/features/order-tool/devcontainer-feature.json"),
                r#"{"id":"order-tool","version":"1.0.0","name":"Order Tool"}"#,
            )
            .unwrap();
            fs::write(
                workspace
                    .root()
                    .join(".devcontainer/features/order-tool/install.sh"),
                r#"
                set -eu
                mkdir -p /usr/local/share
                id -u syncuser >/usr/local/share/decune-feature-syncuser-uid
                test "$(cat /usr/local/share/decune-feature-syncuser-uid)" = 2001
                "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_numeric_image_user_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-numeric-user");
            let image = format!(
                "decune-test/uid-gid-sync-numeric-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_numeric_uid_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some(format!("syncuser:{}", host.gid))
                );

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_named_image_user_numeric_group_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-named-user-numeric-group");
            let image = format!(
                "decune-test/uid-gid-sync-named-user-numeric-group-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_named_uid_numeric_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some(format!("syncuser:{}", host.gid))
                );

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "id -u; id -g".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_rewrites_numeric_remote_user_after_uid_gid_sync() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.uid == 2001 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-numeric-remote-user");
            let image = format!(
                "decune-test/uid-gid-sync-numeric-remote-user-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "2001:2001",
                      "postCreateCommand": "id -u >/tmp/decune-remote-user-ids; id -g >>/tmp/decune-remote-user-ids"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_numeric_uid_gid_user_image(&client, &image).await?;

                let outcome = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!outcome.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-remote-user-ids".to_owned(),
                        ],
                        user: Some("root".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(
                    String::from_utf8(output.stdout)?,
                    format!("{}\n{}\n", host.uid, host.gid)
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_reports_missing_explicit_uid_gid_sync_target_user() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-missing-target-user");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteUser": "missing-sync-user"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(message.contains("Remote user does not exist in container"));
                assert!(message.contains("missing-sync-user"));

                let containers = list_workspace_containers(&client, workspace.id()).await?;
                assert!(
                    !containers
                        .iter()
                        .any(|container| container.name == container_name)
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_host_ids_conflict() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-conflict");
            let image = format!(
                "decune-test/uid-gid-sync-conflict-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_conflict_user_image(&client, &image, host.uid, host.gid).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_host_ids_already_match_but_duplicates_exist() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-duplicate-matching-ids");
            let image = format!(
                "decune-test/uid-gid-sync-duplicate-matching-ids-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_duplicate_matching_host_ids_image(&client, &image, host.uid, host.gid)
                    .await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_gid_conflict_without_target_group_entry() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-missing-target-group");
            let image = format!(
                "decune-test/uid-gid-sync-missing-target-group-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_missing_target_group_conflict_image(&client, &image, host.gid).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_detach_fails_uid_gid_sync_when_old_gid_matches_multiple_groups() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }
        let host = current_host_user_ids();
        if host.uid == 0 || host.gid == 0 || host.gid == 2001 {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-duplicate-old-gid");
            let image = format!(
                "decune-test/uid-gid-sync-duplicate-old-gid-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "remoteUser": "syncuser"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_duplicate_old_gid_image(&client, &image).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("Failed to build Docker image")
                        && message.contains("sync-uid-gid.sh"),
                    "{message}"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
}

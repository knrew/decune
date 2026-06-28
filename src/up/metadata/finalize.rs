use anyhow::Result;
use serde_json::Value as JsonValue;

use crate::{
    devcontainer::features::remove_feature_lock_file,
    docker::{
        client::DockerClient,
        image::{remove_image, tag_image},
    },
    runtime::{
        compose_cli::ComposeConfigService,
        compose_ports::{
            ComposePublishedPortOverride, ComposePublishedPortPlan,
            ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
        },
    },
    up::{
        build::{
            build_feature_layer_image, build_workspace_image_layers,
            plan_requires_final_image_layer, plan_requires_workspace_layer,
            prepare_base_image_for_plan,
        },
        plan::rebuild_up_plan_with_image_metadata_layers,
        types::{ForwardingResolution, MountResolution, UpPlan, UpPlanResolution},
    },
    workspace::Workspace,
};

use super::{
    ImageLookupPreparation, dockerfile_image_metadata_for_plan,
    finalize_mounts_and_resources_for_plan, maybe_auto_add_github_cli_feature_to_plan,
    prepare_feature_metadata_for_plan,
};

pub(in crate::up) async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    existing_container_config_hash: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<FinalizeUpPlanResult> {
    let update_features = options.update_features;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;

    let Some(mut lookup_state) =
        prepare_initial_lookup_image_state(client, &plan, remote_user_image, build_for_lookup)
            .await?
    else {
        return Ok(FinalizeUpPlanResult::new(plan, false));
    };
    let (prepared_plan, prepared_lookup_state) = Box::pin(prepare_lookup_image_after_metadata(
        client,
        workspace,
        plan,
        lookup_state,
        build_for_lookup,
        options,
    ))
    .await?;
    plan = prepared_plan;
    let Some(prepared_lookup_state) = prepared_lookup_state else {
        return Ok(FinalizeUpPlanResult::new(plan, false));
    };
    lookup_state = prepared_lookup_state;
    if !prepare_deferred_workspace_lookup_layer(client, &plan, &mut lookup_state, build_for_lookup)
        .await?
    {
        return Ok(FinalizeUpPlanResult::new(plan, false));
    }

    let lookup = lookup_state.lookup_preparation(remote_user_image, build_for_lookup);
    plan = Box::pin(maybe_auto_add_github_cli_feature_to_plan(
        client,
        workspace,
        plan,
        lookup,
        existing_container_config_hash,
        options,
    ))
    .await?;
    if plan.config.features.is_empty() {
        remove_feature_lock_file(&workspace.root().join(".decune").join("features.lock.toml"))?;
    }
    let finalized = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan,
        &lookup_state.image,
        options,
    ))
    .await?;
    plan = finalized.plan;

    let image_prepared = lookup_state.image_prepared;
    cleanup_lookup_images(client, &plan, lookup_state, build_for_lookup).await?;

    Ok(FinalizeUpPlanResult {
        plan,
        image_prepared,
        compose_published_port_plan: finalized.compose_published_port_plan,
        compose_published_port_override: finalized.compose_published_port_override,
    })
}

struct LookupImageState {
    image: String,
    base_image: Option<String>,
    image_prepared: bool,
    deferred_workspace_layer: bool,
    stale_lookup_images: Vec<String>,
    using_existing_remote_user_image: bool,
}

impl LookupImageState {
    fn from_existing_image(image: &str) -> Self {
        Self {
            image: image.to_owned(),
            base_image: None,
            image_prepared: false,
            deferred_workspace_layer: false,
            stale_lookup_images: Vec::new(),
            using_existing_remote_user_image: true,
        }
    }

    fn from_base_image(
        plan: &UpPlan,
        image_prepared: bool,
        deferred_workspace_layer: bool,
    ) -> Self {
        Self {
            image: plan.base_image.clone(),
            base_image: image_prepared.then(|| plan.base_image.clone()),
            image_prepared,
            deferred_workspace_layer,
            stale_lookup_images: Vec::new(),
            using_existing_remote_user_image: false,
        }
    }

    fn from_feature_layer(plan: &UpPlan) -> Self {
        Self {
            image: plan.image.clone(),
            base_image: Some(plan.base_image.clone()),
            image_prepared: true,
            deferred_workspace_layer: false,
            stale_lookup_images: Vec::new(),
            using_existing_remote_user_image: false,
        }
    }

    const fn lookup_preparation<'a>(
        &'a mut self,
        remote_user_image: Option<&'a str>,
        build_for_lookup: Option<(bool, bool)>,
    ) -> ImageLookupPreparation<'a> {
        ImageLookupPreparation {
            image: &mut self.image,
            remote_user_image,
            base_image: &mut self.base_image,
            image_prepared: &mut self.image_prepared,
            build_options: if self.using_existing_remote_user_image {
                None
            } else {
                build_for_lookup
            },
            command_probe_build_options: build_for_lookup,
        }
    }
}

async fn prepare_initial_lookup_image_state(
    client: &DockerClient,
    plan: &UpPlan,
    remote_user_image: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<Option<LookupImageState>> {
    if let Some(remote_user_image) = remote_user_image {
        return Ok(Some(LookupImageState::from_existing_image(
            remote_user_image,
        )));
    }

    if plan.build_context.is_some() {
        let Some((pull, no_cache)) = build_for_lookup else {
            return Ok(None);
        };
        prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
        return Ok(Some(LookupImageState::from_base_image(
            plan,
            true,
            plan_requires_workspace_layer(plan),
        )));
    }

    if plan_requires_workspace_layer(plan) {
        let Some((pull, no_cache)) = build_for_lookup else {
            return Ok(None);
        };
        prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
        build_feature_layer_image(client, plan, no_cache).await?;
        return Ok(Some(LookupImageState::from_feature_layer(plan)));
    }

    Ok(Some(LookupImageState::from_base_image(plan, false, false)))
}

async fn prepare_lookup_image_after_metadata(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    mut lookup_state: LookupImageState,
    build_for_lookup: Option<(bool, bool)>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<(UpPlan, Option<LookupImageState>)> {
    let dockerfile_metadata =
        dockerfile_image_metadata_for_plan(client, &plan, &lookup_state.image, options.forwarding)
            .await?;
    if dockerfile_metadata.layers.is_empty() {
        return Ok((plan, Some(lookup_state)));
    }

    let skip_global_config = plan.config_layers.global.is_none();
    plan = rebuild_up_plan_with_image_metadata_layers(
        workspace,
        plan,
        dockerfile_metadata.layers,
        options.forwarding == ForwardingResolution::IgnoreDetached
            && dockerfile_metadata.has_forward_ports,
        MountResolution::Resolve,
        UpPlanResolution::new(
            options.forwarding,
            options.update_features,
            skip_global_config,
        ),
    )?;
    plan = prepare_feature_metadata_for_plan(workspace, plan, options.update_features).await?;
    if !refresh_lookup_image_after_metadata(client, &plan, &mut lookup_state, build_for_lookup)
        .await?
    {
        return Ok((plan, None));
    }

    Ok((plan, Some(lookup_state)))
}

async fn refresh_lookup_image_after_metadata(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_state: &mut LookupImageState,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<bool> {
    if !plan_requires_workspace_layer(plan) || lookup_state.using_existing_remote_user_image {
        return Ok(true);
    }

    let Some((pull, no_cache)) = build_for_lookup else {
        return Ok(false);
    };
    if lookup_state.image != plan.base_image {
        lookup_state
            .stale_lookup_images
            .push(lookup_state.image.clone());
    }
    prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
    lookup_state.base_image = Some(plan.base_image.clone());
    build_feature_layer_image(client, plan, no_cache).await?;
    lookup_state.image.clone_from(&plan.image);
    lookup_state.image_prepared = true;
    lookup_state.deferred_workspace_layer = false;
    Ok(true)
}

async fn prepare_deferred_workspace_lookup_layer(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_state: &mut LookupImageState,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<bool> {
    if !lookup_state.deferred_workspace_layer
        || !plan_requires_workspace_layer(plan)
        || lookup_state.using_existing_remote_user_image
    {
        return Ok(true);
    }

    let Some((_, no_cache)) = build_for_lookup else {
        return Ok(false);
    };
    build_feature_layer_image(client, plan, no_cache).await?;
    lookup_state.image.clone_from(&plan.image);
    lookup_state.image_prepared = true;
    Ok(true)
}

async fn cleanup_lookup_images(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_state: LookupImageState,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<()> {
    if lookup_state.image_prepared && plan_requires_final_image_layer(plan) {
        cleanup_lookup_images_after_final_layer(client, plan, &lookup_state, build_for_lookup)
            .await?;
    } else if lookup_state.image_prepared && plan.image != lookup_state.image {
        tag_image(client, &lookup_state.image, &plan.image).await?;
        remove_image(client, &lookup_state.image, false).await?;
    }

    remove_stale_lookup_images(client, plan, &lookup_state).await
}

async fn cleanup_lookup_images_after_final_layer(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_state: &LookupImageState,
    build_for_lookup: Option<(bool, bool)>,
) -> Result<()> {
    if let Some((pull, no_cache)) = build_for_lookup {
        prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
        build_workspace_image_layers(client, plan, no_cache).await?;
    }
    if plan.image != lookup_state.image {
        remove_image(client, &lookup_state.image, false).await?;
    }
    if let Some(lookup_base_image) = &lookup_state.base_image
        && lookup_base_image != &plan.base_image
    {
        remove_image(client, lookup_base_image, false).await?;
    }
    Ok(())
}

async fn remove_stale_lookup_images(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_state: &LookupImageState,
) -> Result<()> {
    for stale_lookup_image in &lookup_state.stale_lookup_images {
        if stale_lookup_image != &plan.image
            && stale_lookup_image != &plan.base_image
            && stale_lookup_image != &lookup_state.image
        {
            remove_image(client, stale_lookup_image, false).await?;
        }
    }
    Ok(())
}

pub(in crate::up) struct FinalizeUpPlanResult {
    pub(in crate::up) plan: UpPlan,
    pub(in crate::up) image_prepared: bool,
    pub(in crate::up) compose_published_port_plan: ComposePublishedPortPlan,
    pub(in crate::up) compose_published_port_override: ComposePublishedPortOverride,
}

impl FinalizeUpPlanResult {
    fn new(plan: UpPlan, image_prepared: bool) -> Self {
        Self {
            plan,
            image_prepared,
            compose_published_port_plan: ComposePublishedPortPlan::default(),
            compose_published_port_override: ComposePublishedPortOverride::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::up) struct ComposePublishedPortFinalization<'a> {
    pub(in crate::up) input: &'a ComposePublishedPortPlanningInput,
    pub(in crate::up) existing_project_published_ports: &'a [ComposePublishedPortReservation],
}

#[derive(Debug, Clone, Copy)]
pub(in crate::up) struct FinalizeUpPlanMountsOptions<'a> {
    pub(in crate::up) forwarding: ForwardingResolution,
    pub(in crate::up) update_features: bool,
    pub(in crate::up) compose_canonical_model: Option<&'a JsonValue>,
    pub(in crate::up) compose_primary_service_user: Option<&'a str>,
    pub(in crate::up) compose_primary_service: Option<&'a ComposeConfigService>,
    pub(in crate::up) compose_published_ports: Option<ComposePublishedPortFinalization<'a>>,
}

impl Default for FinalizeUpPlanMountsOptions<'_> {
    fn default() -> Self {
        Self {
            forwarding: ForwardingResolution::Resolve,
            update_features: false,
            compose_canonical_model: None,
            compose_primary_service_user: None,
            compose_primary_service: None,
            compose_published_ports: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigLayer,
        docker::{
            client::DockerClient,
            container::remove_container,
            image::{PullPolicy, ensure_image, remove_image},
            user::{
                HostPlatform, UidGidSyncNoopReason, UidGidSyncPlan, UidGidSyncTargetKind,
                current_host_user_ids,
            },
        },
        up::{
            ForwardingResolution,
            plan::build_up_plan,
            test_support::{build_uid_gid_user_image, test_workspace, write_devcontainer},
        },
    };

    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_noops_uid_gid_sync_for_root_remote_user() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-root-noop");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "remoteUser": "root"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let client = DockerClient::connect_from_env();

            ensure_image(&client, "alpine:3.20", PullPolicy::Missing)
                .await
                .unwrap();
            let finalized = Box::pin(finalize_up_plan_mounts(
                &client,
                &workspace,
                plan,
                None,
                None,
                Some((false, false)),
                FinalizeUpPlanMountsOptions {
                    forwarding: ForwardingResolution::Resolve,
                    update_features: false,
                    compose_canonical_model: None,
                    compose_primary_service_user: None,
                    compose_primary_service: None,
                    compose_published_ports: None,
                },
            ))
            .await
            .unwrap();
            let plan = finalized.plan;
            let image_prepared = finalized.image_prepared;

            assert!(!image_prepared);
            assert_eq!(plan.image, "alpine:3.20");
            assert_eq!(plan.base_image, "alpine:3.20");
            assert_eq!(
                plan.uid_gid_sync_plan,
                UidGidSyncPlan::Noop {
                    reason: UidGidSyncNoopReason::Root
                }
            );
            assert!(plan.pre_uid_gid_sync_resources.is_none());
            assert!(plan.uid_gid_sync_build_context_dir.is_none());
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_uses_image_user_without_uid_gid_sync_when_metadata_user_is_missing() {
        if HostPlatform::current() != HostPlatform::Linux {
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-uid-gid-sync-image-user-only");
            let image = format!(
                "decune-test/uid-gid-sync-image-user-only-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env();

            let result: anyhow::Result<()> = Box::pin(async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "imageuser", 2001, 2001).await?;

                let finalized = Box::pin(finalize_up_plan_mounts(
                    &client,
                    &workspace,
                    plan,
                    None,
                    None,
                    Some((false, false)),
                    FinalizeUpPlanMountsOptions {
                        forwarding: ForwardingResolution::Resolve,
                        update_features: false,
                        compose_canonical_model: None,
                        compose_primary_service_user: None,
                        compose_primary_service: None,
                        compose_published_ports: None,
                    },
                ))
                .await?;
                let plan = finalized.plan;
                let image_prepared = finalized.image_prepared;

                assert!(!image_prepared);
                assert_eq!(plan.image, image);
                assert_eq!(plan.base_image, image);
                assert_eq!(plan.effective_users.remote_user.user, "imageuser");
                assert_eq!(
                    plan.uid_gid_sync_plan,
                    UidGidSyncPlan::Noop {
                        reason: UidGidSyncNoopReason::NoExplicitUser
                    }
                );
                assert!(plan.pre_uid_gid_sync_resources.is_none());
                assert!(plan.uid_gid_sync_build_context_dir.is_none());

                Ok(())
            })
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[cfg(unix)]
    #[test]
    fn up_plan_finalization_includes_uid_gid_sync_state_in_final_hash_and_image_tag() {
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
            let workspace = test_workspace("docker-up-uid-gid-sync-hash-tag");
            let image = format!(
                "decune-test/uid-gid-sync-hash-tag-{}:latest",
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
            let client = DockerClient::connect_from_env();

            let result: anyhow::Result<()> = Box::pin(async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;
                build_uid_gid_user_image(&client, &image, "syncuser", 2001, 2001).await?;

                let finalized = Box::pin(finalize_up_plan_mounts(
                    &client,
                    &workspace,
                    plan,
                    None,
                    None,
                    Some((false, false)),
                    FinalizeUpPlanMountsOptions {
                        forwarding: ForwardingResolution::Resolve,
                        update_features: false,
                        compose_canonical_model: None,
                        compose_primary_service_user: None,
                        compose_primary_service: None,
                        compose_published_ports: None,
                    },
                ))
                .await?;
                let plan = finalized.plan;
                let image_prepared = finalized.image_prepared;
                let pre_sync_resources = plan
                    .pre_uid_gid_sync_resources
                    .as_ref()
                    .expect("sync plan must preserve pre-sync resources");

                assert!(!image_prepared);
                assert!(matches!(
                    plan.uid_gid_sync_plan,
                    UidGidSyncPlan::Sync { .. }
                ));
                assert_eq!(plan.image, plan.resources.image_tag);
                assert_eq!(plan.base_image, image);
                assert_eq!(
                    plan.resources.labels["decune.config_hash"],
                    plan.resources.config_hash
                );
                assert_eq!(
                    pre_sync_resources.labels["decune.config_hash"],
                    pre_sync_resources.config_hash
                );
                assert_ne!(plan.resources.config_hash, pre_sync_resources.config_hash);
                assert_ne!(plan.resources.image_tag, pre_sync_resources.image_tag);
                assert!(plan.uid_gid_sync_build_context_dir.is_some());

                let UidGidSyncPlan::Sync { target, .. } = &plan.uid_gid_sync_plan else {
                    unreachable!("sync plan was checked above");
                };
                assert_eq!(target.host, host);
                assert_eq!(target.user, "syncuser");
                assert_eq!(target.kind, UidGidSyncTargetKind::RemoteUser);

                Ok(())
            })
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
}

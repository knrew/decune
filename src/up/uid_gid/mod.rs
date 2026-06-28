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
        devcontainer_remote: config_layers
            .devcontainer
            .as_ref()
            .and_then(layer_remote_user),
        devcontainer_container: config_layers
            .devcontainer
            .as_ref()
            .and_then(layer_container_user),
        image_metadata_remote: merged_metadata_remote_user(config_layers),
        image_metadata_container: merged_metadata_container_user(config_layers),
        image_config: None,
    }
}

pub(super) fn effective_user_input_from_plan(plan: &UpPlan) -> EffectiveUserResolveInput<'_> {
    let layer_input = effective_user_input_from_config_layers(&plan.config_layers);
    let devcontainer_remote_user = layer_input
        .devcontainer_remote
        .and(plan.config.devcontainer.remote_user.as_deref());
    let image_metadata_remote_user = if devcontainer_remote_user.is_some() {
        None
    } else {
        layer_input
            .image_metadata_remote
            .and(plan.config.devcontainer.remote_user.as_deref())
    };
    let devcontainer_container_user = layer_input
        .devcontainer_container
        .and(plan.config.devcontainer.container_user.as_deref());
    let image_metadata_container_user = if devcontainer_container_user.is_some() {
        None
    } else {
        layer_input
            .image_metadata_container
            .and(plan.config.devcontainer.container_user.as_deref())
    };

    EffectiveUserResolveInput {
        devcontainer_remote: devcontainer_remote_user,
        devcontainer_container: devcontainer_container_user,
        image_metadata_remote: image_metadata_remote_user,
        image_metadata_container: image_metadata_container_user,
        image_config: None,
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
    if input.devcontainer_remote.is_some()
        || input.image_metadata_remote.is_some()
        || input.devcontainer_container.is_some()
        || input.image_metadata_container.is_some()
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

const fn uid_gid_sync_target_kind_name(kind: UidGidSyncTargetKind) -> &'static str {
    match kind {
        UidGidSyncTargetKind::RemoteUser => "remoteUser",
        UidGidSyncTargetKind::ContainerUser => "containerUser",
    }
}

const fn uid_gid_sync_noop_reason_name(reason: UidGidSyncNoopReason) -> &'static str {
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

pub(super) const fn plan_requires_uid_gid_sync_layer(plan: &UpPlan) -> bool {
    uid_gid_sync_plan_requires_layer(&plan.uid_gid_sync_plan)
}

pub(super) const fn uid_gid_sync_plan_requires_layer(plan: &UidGidSyncPlan) -> bool {
    matches!(plan, UidGidSyncPlan::Sync { .. })
}

pub(super) fn effective_users_depend_on_image_config_user(plan: &UpPlan) -> bool {
    let input = effective_user_input_from_config_layers(&plan.config_layers);
    input.devcontainer_remote.is_none()
        && input.image_metadata_remote.is_none()
        && input.devcontainer_container.is_none()
        && input.image_metadata_container.is_none()
}

const fn plan_requires_workspace_layer(plan: &UpPlan) -> bool {
    plan.feature_install.is_some()
        || !plan.config.features.is_empty()
        || !plan.config.devcontainer.entrypoints.is_empty()
}

#[cfg(test)]
mod failure_scenario_tests;
#[cfg(test)]
mod logic_tests;
#[cfg(test)]
mod sync_scenario_tests;
#[cfg(test)]
mod test_support;

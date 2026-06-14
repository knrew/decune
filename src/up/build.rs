use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::{
    docker::{
        build::{
            DockerBuildInput, DockerBuildOptions, FeatureLayerBuildFeature, FeatureLayerBuildInput,
            UidGidSyncLayerBuildInput, build_image, prepare_feature_layer_build_context,
            prepare_uid_gid_sync_layer_build_context,
        },
        client::DockerClient,
        image::{PullPolicy, ensure_image},
        user::{UidGidSyncPlan, image_config_user, uid_gid_sync_runtime_user},
    },
    up::{
        metadata::warn_about_unsupported_dockerfile_image_metadata,
        plan::config_requires_workspace_layer,
        types::UpPlan,
        uid_gid::{
            plan_requires_uid_gid_sync_layer, pre_uid_gid_sync_layer_resources,
            uid_gid_sync_base_image,
        },
    },
};

pub(in crate::up) async fn prepare_base_image_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
) -> Result<()> {
    if let Some(context) = plan.build_context.clone() {
        let mut build_options = plan.build_options.clone();
        build_options.pull = pull;
        build_options.no_cache = no_cache;
        build_image(
            client,
            DockerBuildInput {
                image_tag: plan.base_image.clone(),
                labels: plan.resources.labels.clone().into_iter().collect(),
                context,
                options: build_options,
            },
        )
        .await?;
        warn_about_unsupported_dockerfile_image_metadata(client, &plan.base_image).await?;
    } else {
        ensure_image(
            client,
            &plan.base_image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }

    Ok(())
}

pub(in crate::up) async fn build_feature_layer_image(
    client: &DockerClient,
    plan: &UpPlan,
    no_cache: bool,
    pull: bool,
) -> Result<()> {
    if !plan_requires_workspace_layer(plan) {
        return Ok(());
    }
    let feature_build_context_dir = plan
        .feature_build_context_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Feature build context directory was not prepared"))?;
    let final_user = image_config_user(client, &plan.base_image)
        .await?
        .unwrap_or_else(|| "root".to_owned());
    let install_env = feature_install_env(plan, &final_user);
    let devcontainer_id = plan
        .pre_uid_gid_sync_resources
        .as_ref()
        .unwrap_or(&plan.resources)
        .labels
        .get("decune.workspace_id")
        .cloned()
        .context("Feature layer build requires a workspace id label")?;
    let context = prepare_feature_layer_build_context(&FeatureLayerBuildInput {
        base_image: plan.base_image.clone(),
        devcontainer_id,
        final_user,
        entrypoints: plan.config.devcontainer.entrypoints.clone(),
        install_env,
        context_dir: feature_build_context_dir.clone(),
        features: plan
            .feature_install
            .as_ref()
            .map(|feature_install| {
                feature_install
                    .entries
                    .iter()
                    .map(|entry| FeatureLayerBuildFeature {
                        id: entry.feature.canonical_id.clone(),
                        source_dir: entry.source_dir.clone(),
                        option_env: entry.option_env.clone(),
                        container_env: entry.container_env.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })?;
    build_image(
        client,
        DockerBuildInput {
            image_tag: feature_layer_image(plan),
            labels: pre_uid_gid_sync_layer_resources(plan)
                .labels
                .clone()
                .into_iter()
                .collect(),
            context,
            options: DockerBuildOptions {
                no_cache,
                pull,
                ..DockerBuildOptions::default()
            },
        },
    )
    .await
}

async fn build_uid_gid_sync_layer_image(
    client: &DockerClient,
    plan: &UpPlan,
    no_cache: bool,
    pull: bool,
) -> Result<()> {
    let UidGidSyncPlan::Sync { target, container } = &plan.uid_gid_sync_plan else {
        return Ok(());
    };
    let context_dir = plan
        .uid_gid_sync_build_context_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("UID/GID sync build context directory was not prepared"))?;
    let base_image = uid_gid_sync_base_image(plan);
    let final_user = image_config_user(client, &base_image)
        .await?
        .unwrap_or_else(|| "root".to_owned());
    let final_user = uid_gid_sync_runtime_user(&final_user, &plan.uid_gid_sync_plan)?;
    let context = prepare_uid_gid_sync_layer_build_context(&UidGidSyncLayerBuildInput {
        base_image,
        final_user,
        target_user: container.name.clone(),
        old_uid: container.uid,
        old_gid: container.gid,
        new_uid: target.host.uid,
        new_gid: target.host.gid,
        context_dir: context_dir.clone(),
    })?;
    build_image(
        client,
        DockerBuildInput {
            image_tag: plan.image.clone(),
            labels: plan.resources.labels.clone().into_iter().collect(),
            context,
            options: DockerBuildOptions {
                no_cache,
                pull,
                ..DockerBuildOptions::default()
            },
        },
    )
    .await
}

pub(in crate::up) async fn build_workspace_image_layers(
    client: &DockerClient,
    plan: &UpPlan,
    no_cache: bool,
    pull: bool,
) -> Result<()> {
    if plan_requires_workspace_layer(plan) {
        build_feature_layer_image(client, plan, no_cache, pull).await?;
    }
    if plan_requires_uid_gid_sync_layer(plan) {
        build_uid_gid_sync_layer_image(client, plan, no_cache, pull).await?;
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::up) fn feature_layer_image(plan: &UpPlan) -> String {
    feature_layer_image_inner(plan)
}

#[cfg(not(test))]
fn feature_layer_image(plan: &UpPlan) -> String {
    feature_layer_image_inner(plan)
}

fn feature_layer_image_inner(plan: &UpPlan) -> String {
    if plan_requires_uid_gid_sync_layer(plan) {
        pre_uid_gid_sync_layer_resources(plan).image_tag.clone()
    } else {
        plan.image.clone()
    }
}

pub(in crate::up) fn plan_requires_workspace_layer(plan: &UpPlan) -> bool {
    plan.feature_install.is_some() || config_requires_workspace_layer(&plan.config)
}

pub(in crate::up) fn plan_requires_final_image_layer(plan: &UpPlan) -> bool {
    plan_requires_workspace_layer(plan) || plan_requires_uid_gid_sync_layer(plan)
}

fn feature_install_env(plan: &UpPlan, image_user: &str) -> BTreeMap<String, String> {
    let container_user = plan
        .config
        .devcontainer
        .container_user
        .as_deref()
        .unwrap_or(image_user)
        .to_owned();
    let remote_user = plan
        .config
        .devcontainer
        .remote_user
        .clone()
        .unwrap_or_else(|| container_user.clone());

    BTreeMap::from([
        ("_CONTAINER_USER".to_owned(), container_user),
        ("_CONTAINER_USER_HOME".to_owned(), String::new()),
        ("_REMOTE_USER".to_owned(), remote_user),
        ("_REMOTE_USER_HOME".to_owned(), String::new()),
    ])
}

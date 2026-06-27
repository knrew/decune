use std::path::Path;

use anyhow::{Result, bail};

use crate::{
    config::resolved::{ResolvedConfig, ResolvedDevcontainerSource},
    docker::{
        build::{DockerBuildOptions, ResolvedBuildContext, resolve_build_context},
        resource::DockerResources,
        user::UidGidSyncPlan,
    },
};

pub(in crate::up) fn final_image_source(
    config: &ResolvedConfig,
    resources: &DockerResources,
    uid_gid_sync_plan: &UidGidSyncPlan,
) -> Result<String> {
    if config_requires_workspace_layer(config)
        || uid_gid_sync_plan_requires_layer(uid_gid_sync_plan)
    {
        return Ok(resources.image_tag.clone());
    }

    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        Some(ResolvedDevcontainerSource::Compose(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

pub(in crate::up) fn base_image_source(
    config: &ResolvedConfig,
    resources: &DockerResources,
    _uid_gid_sync_plan: &UidGidSyncPlan,
) -> Result<String> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_))
            if config_requires_workspace_layer(config) =>
        {
            Ok(format!("{}-base", resources.image_tag))
        }
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        Some(ResolvedDevcontainerSource::Compose(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

pub(super) fn dockerfile_build_input(
    workspace_root: &Path,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<(Option<ResolvedBuildContext>, DockerBuildOptions)> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Dockerfile(build)) => Ok((
            Some(resolve_build_context(
                workspace_root,
                devcontainer_file,
                build,
            )?),
            DockerBuildOptions {
                build_args: build.args.clone(),
                options: build.options.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
                ..DockerBuildOptions::default()
            },
        )),
        _ => Ok((None, DockerBuildOptions::default())),
    }
}

pub(in crate::up) fn config_requires_workspace_layer(config: &ResolvedConfig) -> bool {
    !config.features.is_empty() || !config.devcontainer.entrypoints.is_empty()
}

fn uid_gid_sync_plan_requires_layer(plan: &UidGidSyncPlan) -> bool {
    matches!(plan, UidGidSyncPlan::Sync { .. })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{
        config::{ConfigLayer, resolved::ResolvedDevcontainerSource, types::MountType},
        up::{
            plan::build_up_plan,
            test_support::{test_workspace, write_devcontainer},
        },
        workspace::Workspace,
    };

    #[test]
    fn build_up_plan_uses_image_source_and_default_workspace_mount() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Image Plan!");
        fs::create_dir(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"image":"alpine:3.20"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert_eq!(plan.base_image, "alpine:3.20");
        assert_eq!(plan.workspace_folder, "/workspaces/Image Plan!");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(root.to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/Image Plan!");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
    }

    #[test]
    fn build_up_plan_records_image_source_labels_and_workspace_mount() {
        let workspace = test_workspace("image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceFolder": "/workspace"
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert!(plan.build_context.is_none());
        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/image-plan");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
        assert!(matches!(
            plan.config.devcontainer.source,
            Some(ResolvedDevcontainerSource::Image(ref image)) if image == "alpine:3.20"
        ));
        assert_eq!(
            plan.resources.labels["devcontainer.config_file"],
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string()
        );
    }

    #[test]
    fn build_up_plan_uses_dockerfile_source_and_build_context() {
        let workspace = test_workspace("dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile",
            "args": {
              "VARIANT": "bookworm"
            },
            "options": [
              "--platform=linux/amd64",
              "--network",
              "host"
            ],
            "target": "dev",
            "cacheFrom": "type=registry,ref=example.test/cache:latest"
          }
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, plan.resources.image_tag);
        let build_context = plan
            .build_context
            .expect("build context should be resolved");
        assert_eq!(
            build_context.context_dir,
            workspace.root().join(".devcontainer")
        );
        assert_eq!(
            build_context.dockerfile_path,
            workspace.root().join(".devcontainer/Dockerfile")
        );
        assert_eq!(
            build_context.dockerfile_in_context,
            PathBuf::from("Dockerfile")
        );
        assert_eq!(
            plan.build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert_eq!(plan.build_options.target.as_deref(), Some("dev"));
        assert_eq!(
            plan.build_options.options,
            vec!["--platform=linux/amd64", "--network", "host"]
        );
        assert_eq!(
            plan.build_options.cache_from,
            vec!["type=registry,ref=example.test/cache:latest"]
        );
        assert!(!plan.build_options.no_cache);
        assert!(!plan.build_options.pull);
    }

    #[test]
    fn build_up_plan_hash_changes_when_dockerfile_content_changes() {
        let workspace = test_workspace("dockerfile-hash-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile"
          }
        }
        "#,
        );

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\nRUN true\n",
        )
        .unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
        assert_ne!(first.image, second.image);
    }
}

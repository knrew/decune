use std::path::Path;

use anyhow::{Result, bail};

use crate::{
    config::resolved::{ResolvedConfig, ResolvedDevcontainerSource},
    runtime::compose_cli::ComposeProjectPlan,
    workspace::Workspace,
};

pub(super) fn compose_project_plan(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<Option<ComposeProjectPlan>> {
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &config.devcontainer.source else {
        return Ok(None);
    };
    let devcontainer_dir = devcontainer_file.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to resolve devcontainer metadata directory: {}",
            devcontainer_file.display()
        )
    })?;

    ComposeProjectPlan::resolve(workspace, devcontainer_dir, &compose.files).map(Some)
}

pub(super) fn validate_service_qualified_forward_ports(config: &ResolvedConfig) -> Result<()> {
    if matches!(
        config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    ) {
        return Ok(());
    }

    if let Some(port) = config
        .ports
        .entries
        .iter()
        .find(|port| port.service.is_some())
    {
        let service = port.service.as_deref().unwrap_or_default();
        bail!(
            "Service-qualified port forwarding is only supported in Docker Compose mode: {service}:{}",
            port.container
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        config::ConfigLayer,
        up::{
            plan::build_up_plan,
            test_support::{test_workspace, write_devcontainer},
        },
        workspace::Workspace,
    };

    #[test]
    fn build_up_plan_adds_compose_project_plan_for_compose_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Compose Plan");
        fs::create_dir_all(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(devcontainer_dir.join("compose.yaml"), "services: {}\n").unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let compose = plan
            .compose_project
            .as_ref()
            .expect("compose source should produce a compose project plan");

        assert_eq!(
            compose.project_name(),
            format!("decune-compose-plan-{}", workspace.id())
        );
        assert_eq!(
            compose.generated_override_path(),
            workspace.paths().state_dir().join("compose.override.yaml")
        );
    }

    #[test]
    fn build_up_plan_config_hash_changes_when_compose_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Compose Hash");
        fs::create_dir_all(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let compose_file = devcontainer_dir.join("compose.yaml");
        fs::write(&compose_file, "services: {}\n").unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(&compose_file, "services:\n  app:\n    image: alpine:3.20\n").unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_without_compose_source() {
        let workspace = test_workspace("service-forward-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": ["db:5432"]
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_with_dockerfile_source() {
        let workspace = test_workspace("service-forward-port-dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile"
          },
          "forwardPorts": ["db:5432"]
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }

    #[test]
    fn build_up_plan_rejects_service_qualified_decune_ports_without_compose_source() {
        let workspace = test_workspace("service-decune-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[ports]]
service = "db"
container = 5432
"#,
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }
}

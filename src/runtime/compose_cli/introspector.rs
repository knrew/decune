use anyhow::Result;

use crate::runtime::compose_ports::{
    ComposePublishedPortPlanningInput, compose_published_port_planning_input,
};

use super::{
    adapter::DockerComposeCli,
    command_plan::ComposeCommandPlan,
    config::{ComposeConfigModel, ComposeConfigOutput, ComposeServiceValidation},
    project_plan::ComposeProjectPlan,
    ps::{ComposePsContainer, resolve_compose_container},
};

#[derive(Clone)]
pub(crate) struct ComposeIntrospector {
    cli: DockerComposeCli,
}

impl Default for ComposeIntrospector {
    fn default() -> Self {
        Self::new(DockerComposeCli::default())
    }
}

impl ComposeIntrospector {
    pub(crate) const fn new(cli: DockerComposeCli) -> Self {
        Self { cli }
    }

    pub(crate) async fn user_config_model(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<ComposeConfigModel> {
        Ok(self.user_config(project, validation).await?.model)
    }

    pub(crate) async fn user_config(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<ComposeConfigOutput> {
        let output = self
            .cli
            .config_output(&project.command_plan_without_generated_override())
            .await?;
        output.model.validate_services(validation)?;
        Ok(output)
    }

    pub(crate) async fn user_config_for_services(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
        services: &[String],
    ) -> Result<ComposeConfigOutput> {
        let output = self
            .cli
            .config_output_for_services(
                &project.command_plan_without_generated_override(),
                services,
            )
            .await?;
        output.model.validate_services(validation)?;
        Ok(output)
    }

    pub(crate) async fn user_published_port_planning_input(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
        services: &[String],
    ) -> Result<ComposePublishedPortPlanningInput> {
        let output = self
            .user_config_for_services(project, validation, services)
            .await?;
        Ok(compose_published_port_planning_input(
            &output.model,
            &output.published_port_entries,
            validation.primary_service,
            services,
        ))
    }

    #[cfg(test)]
    pub(crate) async fn config_model_with_generated_override(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<ComposeConfigModel> {
        let output = self
            .cli
            .config_output(&project.command_plan_with_generated_override())
            .await?;
        output.model.validate_services(validation)?;
        Ok(output.model)
    }

    pub(crate) async fn resolve_service_container(
        &self,
        project: &ComposeCommandPlan,
        service: &str,
    ) -> Result<ComposePsContainer> {
        let containers = self.cli.ps_json(project, service).await?;
        resolve_compose_container(&project.project_name, service, containers)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::runtime::command::{FakeRuntimeCommand, RuntimeOutput};

    use super::super::{
        adapter::DockerComposeCli,
        config::ComposeServiceValidation,
        project_plan::ComposeProjectPlan,
        test_support::{fixture_workspace, runtime_output, write_compose_file},
    };
    use super::*;

    #[test]
    fn compose_introspector_builds_active_published_port_planning_input() {
        let (_temp, workspace) = fixture_workspace("active-port-planning");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        let project =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();
        let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
            br#"{
                    "services": {
                        "app": {
                            "image": "alpine:3.20",
                            "ports": [{"target": 3000, "published": "3000"}]
                        },
                        "db": {
                            "image": "alpine:3.20",
                            "ports": [{"target": 5432, "published": "5432"}]
                        }
                    }
                }"#,
        ))]);
        let introspector =
            ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
        let run_services = vec!["db".to_owned()];
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: Some(&run_services),
            workspace_folder: "/workspace",
            project_name: project.project_name(),
        };
        let selected_services = vec!["app".to_owned(), "db".to_owned()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let input = runtime
            .block_on(introspector.user_published_port_planning_input(
                &project,
                &validation,
                &selected_services,
            ))
            .unwrap();

        assert_eq!(input.port_entries.len(), 2);
        assert_eq!(
            input.services.ordered_services_for_planning(),
            ["app", "db"]
        );
        assert_eq!(
            runner.commands()[0]
                .args_vec()
                .iter()
                .rev()
                .take(4)
                .collect::<Vec<_>>(),
            vec!["db", "app", "json", "--format"]
        );
    }

    #[test]
    fn compose_introspector_includes_dependency_published_ports_from_config_output() {
        let (_temp, workspace) = fixture_workspace("dependency-port-planning");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        let project =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();
        let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
            br#"{
                    "services": {
                        "app": {
                            "image": "alpine:3.20",
                            "depends_on": {"db": {"condition": "service_started", "required": true}}
                        },
                        "db": {
                            "image": "alpine:3.20",
                            "ports": [{"target": 5432, "published": "5432"}]
                        }
                    }
                }"#,
        ))]);
        let introspector =
            ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: project.project_name(),
        };
        let selected_services = vec!["app".to_owned()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let input = runtime
            .block_on(introspector.user_published_port_planning_input(
                &project,
                &validation,
                &selected_services,
            ))
            .unwrap();

        assert_eq!(input.port_entries.len(), 1);
        assert_eq!(input.port_entries[0].service, "db");
        assert_eq!(
            input.services.ordered_services_for_planning(),
            ["app", "db"]
        );
        assert_eq!(
            runner.commands()[0]
                .args_vec()
                .iter()
                .rev()
                .take(3)
                .collect::<Vec<_>>(),
            vec!["app", "json", "--format"]
        );
    }
    #[test]
    fn compose_introspection_reads_user_and_generated_config_paths() {
        let (_temp, workspace) = fixture_workspace("introspection-paths");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        let project =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();
        let runner = FakeRuntimeCommand::new(vec![
            Ok(RuntimeOutput {
                stdout: br#"{"services":{"app":{"image":"generated:latest"}}}"#.to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            }),
            Ok(RuntimeOutput {
                stdout: br#"{"services":{"app":{"image":"alpine:3.20"}}}"#.to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            }),
        ]);
        let introspector =
            ComposeIntrospector::new(DockerComposeCli::new(std::sync::Arc::new(runner.clone())));
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: project.project_name(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let user_model = runtime
            .block_on(introspector.user_config_model(&project, &validation))
            .unwrap();
        let generated_model = runtime
            .block_on(introspector.config_model_with_generated_override(&project, &validation))
            .unwrap();
        let commands = runner.commands();

        assert_eq!(
            user_model
                .service("app")
                .and_then(|service| service.image.as_deref()),
            Some("alpine:3.20")
        );
        assert_eq!(
            generated_model
                .service("app")
                .and_then(|service| service.image.as_deref()),
            Some("generated:latest")
        );
        assert!(
            !commands[0]
                .args_vec()
                .contains(&project.generated_override_path().display().to_string())
        );
        assert!(
            commands[1]
                .args_vec()
                .contains(&project.generated_override_path().display().to_string())
        );
    }
}

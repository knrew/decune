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
    pub(crate) fn new(cli: DockerComposeCli) -> Self {
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

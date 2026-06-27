use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value as JsonValue;

use crate::runtime::{
    command::{RuntimeCommand, RuntimeCommandRunner, TokioRuntimeCommand, ensure_success},
    compose_ports::{
        ComposePublishedPortStartupDiagnostics, classify_compose_published_port_startup_failure,
        classify_compose_published_ports, compose_published_port_invalid_config_error,
    },
};

use super::{
    capabilities::ComposeCliCapabilities,
    command_plan::{
        ComposeBuildOptions, ComposeCommandPlan, ComposeDownOptions, ComposePullOptions,
        ComposeStopOptions, ComposeUpOptions, compose_build_command, compose_cmd,
        compose_config_command, compose_down_command, compose_pull_command, compose_stop_command,
        compose_up_command,
    },
    config::ComposeConfigOutput,
    ps::{ComposePsContainer, parse_compose_ps_json},
};

#[derive(Clone)]
pub(crate) struct DockerComposeCli {
    runner: Arc<dyn RuntimeCommandRunner>,
}

fn ensure_compose_config_success(
    project_name: &str,
    command: &RuntimeCommand,
    output: &crate::runtime::command::RuntimeOutput,
) -> Result<()> {
    if output.exit_code == 0 {
        return Ok(());
    }

    let stderr = command.redact_output(&output.stderr_string_lossy());
    if compose_config_error_mentions_port_syntax(&stderr) {
        return Err(compose_published_port_invalid_config_error(project_name, &stderr).into());
    }

    ensure_success("read Docker Compose config", project_name, command, output)
}

fn compose_config_error_mentions_port_syntax(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("invalid hostport")
        || lower.contains("invalid host port")
        || lower.contains("invalid ip address")
        || lower.contains("invalid port")
        || lower.contains("ports must be")
}

fn ensure_compose_up_success(
    project_name: &str,
    command: &RuntimeCommand,
    output: &crate::runtime::command::RuntimeOutput,
    diagnostics: Option<ComposePublishedPortStartupDiagnostics<'_>>,
) -> Result<()> {
    if output.exit_code == 0 {
        return Ok(());
    }

    let stderr = command.redact_output(&output.stderr_string_lossy());
    if let Some(diagnostics) = diagnostics
        && let Some(diagnostic) =
            classify_compose_published_port_startup_failure(&stderr, diagnostics)
    {
        return Err(diagnostic.into());
    }

    ensure_success(
        "start Docker Compose project",
        project_name,
        command,
        output,
    )
}

impl Default for DockerComposeCli {
    fn default() -> Self {
        Self::new(Arc::new(TokioRuntimeCommand))
    }
}

impl DockerComposeCli {
    pub(crate) fn new(runner: Arc<dyn RuntimeCommandRunner>) -> Self {
        Self { runner }
    }

    pub(crate) async fn capabilities(&self) -> Result<ComposeCliCapabilities> {
        let version_short = self.probe_version_short().await?;
        let config_help = self.probe_help("config").await?;
        let ps_help = self.probe_help("ps").await?;
        let build_help = self.probe_help("build").await?;
        let pull_help = self.probe_help("pull").await?;
        let up_help = self.probe_help("up").await?;

        Ok(ComposeCliCapabilities::from_help_outputs(
            version_short,
            &config_help,
            &ps_help,
            &build_help,
            &pull_help,
            &up_help,
        ))
    }

    pub(crate) async fn ensure_required_capabilities(&self) -> Result<ComposeCliCapabilities> {
        let capabilities = self.capabilities().await?;
        capabilities.ensure_required()?;
        Ok(capabilities)
    }

    async fn probe_version_short(&self) -> Result<Option<String>> {
        let command = compose_cmd(["version", "--short"]);
        let output = self.runner.run_capture(command).await?;
        if output.exit_code == 0 {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok((!version.is_empty()).then_some(version));
        }

        let fallback = compose_cmd(["version"]);
        let fallback_output = self.runner.run_capture(fallback.clone()).await?;
        ensure_success(
            "read Docker Compose version",
            "compose",
            &fallback,
            &fallback_output,
        )?;
        Ok(None)
    }

    async fn probe_help(&self, subcommand: &str) -> Result<String> {
        let command = compose_cmd([]).arg(subcommand).arg("--help");
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "probe Docker Compose required capabilities",
            subcommand,
            &command,
            &output,
        )?;
        let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            help.push('\n');
            help.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(help)
    }

    pub(crate) async fn config_output(
        &self,
        project: &ComposeCommandPlan,
    ) -> Result<ComposeConfigOutput> {
        self.config_output_for_services(project, &[]).await
    }

    pub(crate) async fn config_output_for_services(
        &self,
        project: &ComposeCommandPlan,
        services: &[String],
    ) -> Result<ComposeConfigOutput> {
        let command = compose_config_command(project, services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_compose_config_success(&project.project_name, &command, &output)?;
        let canonical_model: JsonValue = serde_json::from_slice(&output.stdout).map_err(|error| {
            anyhow!(
                "Failed to parse Docker Compose config JSON for project {} from files {}: {error}",
                project.project_name,
                project.file_list()
            )
        })?;
        let model = serde_json::from_value(canonical_model.clone()).map_err(|error| {
            anyhow!(
                "Failed to parse Docker Compose config model for project {} from files {}: {error}",
                project.project_name,
                project.file_list()
            )
        })?;
        let published_port_entries = classify_compose_published_ports(&model);
        Ok(ComposeConfigOutput {
            model,
            canonical_model,
            published_port_entries,
        })
    }

    pub(crate) async fn build(
        &self,
        project: &ComposeCommandPlan,
        options: ComposeBuildOptions,
        services: &[String],
    ) -> Result<()> {
        let command = compose_build_command(project, options, services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "build Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn pull(
        &self,
        project: &ComposeCommandPlan,
        options: ComposePullOptions,
        services: &[String],
    ) -> Result<()> {
        let command = compose_pull_command(project, options, services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "pull Docker Compose service images",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn up(
        &self,
        project: &ComposeCommandPlan,
        options: ComposeUpOptions,
        services: &[String],
        diagnostics: Option<ComposePublishedPortStartupDiagnostics<'_>>,
    ) -> Result<()> {
        let command = compose_up_command(project, options, services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_compose_up_success(&project.project_name, &command, &output, diagnostics)
    }

    pub(crate) async fn stop(
        &self,
        project: &ComposeCommandPlan,
        options: ComposeStopOptions,
        services: &[String],
    ) -> Result<()> {
        let command = compose_stop_command(project, options, services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "stop Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn down(
        &self,
        project: &ComposeCommandPlan,
        options: ComposeDownOptions,
    ) -> Result<()> {
        let command = compose_down_command(project, options);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "remove Docker Compose project",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn ps_json(
        &self,
        project: &ComposeCommandPlan,
        service: &str,
    ) -> Result<Vec<ComposePsContainer>> {
        let command = project.command(["ps", "--format", "json"]).arg(service);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "list Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )?;
        parse_compose_ps_json(&output.stdout, &project.project_name, service)
    }
}

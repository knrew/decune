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
    if let Some(diagnostics) = diagnostics {
        match classify_compose_published_port_startup_failure(&stderr, diagnostics) {
            Ok(Some(diagnostic)) | Err(diagnostic) => return Err(diagnostic.into()),
            Ok(None) => {}
        }
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
        let command = compose_build_command(project, &options, services);
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::runtime::compose_ports::diagnostics::COMPOSE_PUBLISHED_PORT_COLLISION;
    use crate::runtime::{
        command::{FakeRuntimeCommand, RuntimeOutput},
        compose_cli::config::ComposeConfigModel,
        compose_ports::{
            ComposePortEligibility, ComposePublishedPortPlan,
            ComposePublishedPortStartupDiagnostics, classify_compose_published_ports,
            compose_published_port_planning_input,
        },
    };

    use super::super::{
        command_plan::ComposeUpOptions,
        test_support::{lifecycle_command_plan, runtime_error_output, runtime_output},
    };
    use super::*;

    #[test]
    fn compose_config_output_includes_published_port_classification() {
        let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
            br#"{
                    "services": {
                        "app": {
                            "image": "alpine:3.20",
                            "ports": [
                                {
                                    "target": 3000,
                                    "published": "3000",
                                    "protocol": "tcp"
                                }
                            ]
                        }
                    }
                }"#,
        ))]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let output = runtime
            .block_on(cli.config_output(&lifecycle_command_plan()))
            .unwrap();

        assert_eq!(output.published_port_entries.len(), 1);
        assert_eq!(output.published_port_entries[0].service, "app");
        assert_eq!(
            output.published_port_entries[0].eligibility,
            ComposePortEligibility::EligibleFixedTcp
        );
    }

    #[test]
    fn compose_config_output_classifies_invalid_port_syntax_errors() {
        let runner = FakeRuntimeCommand::new(vec![Ok(crate::runtime::command::RuntimeOutput {
            stdout: Vec::new(),
            stderr: b"invalid IP address: 999.999.999.999\n".to_vec(),
            exit_code: 1,
        })]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(cli.config_output(&lifecycle_command_plan()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("compose_published_port_invalid"));
        assert!(error.contains("invalid IP address"));
    }

    #[test]
    fn compose_up_classifies_published_port_startup_failures() {
        let runner = FakeRuntimeCommand::new(vec![Ok(runtime_error_output(
            "Error response from daemon: Bind for 0.0.0.0:3000 failed: port is already allocated",
        ))]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner));
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "ports": [{"target": 3000, "published": "3000"}]
                }
            }
        }))
        .unwrap();
        let entries = classify_compose_published_ports(&model);
        let input = compose_published_port_planning_input(&model, &entries, "app", &[]);
        let plan = ComposePublishedPortPlan::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(cli.up(
                &lifecycle_command_plan(),
                ComposeUpOptions::default(),
                &[],
                Some(ComposePublishedPortStartupDiagnostics {
                    input: &input,
                    plan: &plan,
                    planning_active: false,
                }),
            ))
            .unwrap_err()
            .to_string();

        assert!(error.contains(COMPOSE_PUBLISHED_PORT_COLLISION));
        assert!(error.contains("service: `app`"));
        assert!(!error.contains("Failed to start Docker Compose project"));
    }

    #[test]
    fn compose_config_output_for_services_passes_service_args() {
        let runner = FakeRuntimeCommand::new(vec![Ok(runtime_output(
            br#"{
                    "services": {
                        "app": {
                            "image": "alpine:3.20"
                        },
                        "db": {
                            "image": "alpine:3.20"
                        }
                    }
                }"#,
        ))]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let services = vec!["app".to_owned(), "db".to_owned()];

        runtime
            .block_on(cli.config_output_for_services(&lifecycle_command_plan(), &services))
            .unwrap();

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
    fn compose_capability_probe_runs_version_and_help_commands() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(runtime_output("--force-recreate --remove-orphans")),
            Ok(runtime_output(
                "--policy string --ignore-buildable --include-deps",
            )),
            Ok(runtime_output("--with-dependencies")),
            Ok(runtime_output("--format string")),
            Ok(runtime_output("--format string")),
            Ok(runtime_output("2.40.0\n")),
        ]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let capabilities = runtime
            .block_on(cli.ensure_required_capabilities())
            .unwrap();
        let commands = runner.commands();

        assert!(capabilities.build.with_dependencies);
        assert_eq!(commands[0].args_vec(), &["compose", "version", "--short"]);
        assert_eq!(commands[1].args_vec(), &["compose", "config", "--help"]);
        assert_eq!(commands[2].args_vec(), &["compose", "ps", "--help"]);
        assert_eq!(commands[3].args_vec(), &["compose", "build", "--help"]);
        assert_eq!(commands[4].args_vec(), &["compose", "pull", "--help"]);
        assert_eq!(commands[5].args_vec(), &["compose", "up", "--help"]);
        assert!(
            commands
                .iter()
                .all(|command| command.current_dir_path().is_none())
        );
    }

    #[test]
    fn compose_capability_probe_does_not_require_version_short() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(runtime_output("--force-recreate --remove-orphans")),
            Ok(runtime_output(
                "--policy string --ignore-buildable --include-deps",
            )),
            Ok(runtime_output("--with-dependencies")),
            Ok(runtime_output("--format string")),
            Ok(runtime_output("--format string")),
            Ok(runtime_output("Docker Compose version v2.40.0\n")),
            Ok(runtime_error_output("unknown flag: --short")),
        ]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let capabilities = runtime
            .block_on(cli.ensure_required_capabilities())
            .unwrap();
        let commands = runner.commands();

        assert_eq!(capabilities.version_short, None);
        assert_eq!(commands[0].args_vec(), &["compose", "version", "--short"]);
        assert_eq!(commands[1].args_vec(), &["compose", "version"]);
        assert_eq!(commands[2].args_vec(), &["compose", "config", "--help"]);
    }
    #[test]
    fn docker_compose_cli_reads_typed_config_and_ps_json() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(RuntimeOutput {
                stdout: br#"[{"ID":"abc123","Name":"project-app-1","Service":"app"}]"#.to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            }),
            Ok(RuntimeOutput {
                stdout: br#"{"services":{"app":{"image":"alpine:3.20"}}}"#.to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
            }),
        ]);
        let cli = DockerComposeCli::new(std::sync::Arc::new(runner.clone()));
        let command_plan = ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
            env: BTreeMap::new(),
            redactions: Vec::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let config = runtime
            .block_on(cli.config_output(&command_plan))
            .unwrap()
            .model;
        let ps = runtime.block_on(cli.ps_json(&command_plan, "app")).unwrap();
        let commands = runner.commands();

        assert!(config.has_service("app"));
        assert_eq!(ps.len(), 1);
        assert_eq!(
            commands[0].args_vec().last().map(String::as_str),
            Some("json")
        );
        assert_eq!(
            commands[1].args_vec(),
            &[
                "compose",
                "--project-name",
                "decune-project-abc123def456",
                "--project-directory",
                "/workspace",
                "-f",
                "/workspace/compose.yaml",
                "ps",
                "--format",
                "json",
                "app",
            ]
        );
    }
}

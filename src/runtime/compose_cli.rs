#![allow(dead_code)]

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use serde_json::Value;

use crate::runtime::command::{
    RuntimeCommand, RuntimeCommandRunner, RuntimeOutput, TokioRuntimeCommand, ensure_success,
};

#[derive(Clone)]
pub(crate) struct DockerComposeCli {
    runner: Arc<dyn RuntimeCommandRunner>,
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

    pub(crate) async fn version(&self) -> Result<RuntimeOutput> {
        let command = compose_cmd([]).arg("version");
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success("read Docker Compose version", "compose", &command, &output)?;
        Ok(output)
    }

    pub(crate) async fn config(&self, project: &ComposeProject) -> Result<Value> {
        let command = project.command(["config", "--format", "json"]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "read Docker Compose config",
            &project.name,
            &command,
            &output,
        )?;
        serde_json::from_slice(&output.stdout).map_err(Into::into)
    }

    pub(crate) async fn build(&self, project: &ComposeProject, services: &[String]) -> Result<()> {
        let command = project.command(["build"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "build Docker Compose services",
            &project.name,
            &command,
            &output,
        )
    }

    pub(crate) async fn up(&self, project: &ComposeProject, services: &[String]) -> Result<()> {
        let command = project.command(["up", "-d"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "start Docker Compose project",
            &project.name,
            &command,
            &output,
        )
    }

    pub(crate) async fn stop(&self, project: &ComposeProject, services: &[String]) -> Result<()> {
        let command = project.command(["stop"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "stop Docker Compose services",
            &project.name,
            &command,
            &output,
        )
    }

    pub(crate) async fn down(&self, project: &ComposeProject) -> Result<()> {
        let command = project.command(["down"]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "remove Docker Compose project",
            &project.name,
            &command,
            &output,
        )
    }

    pub(crate) async fn ps(&self, project: &ComposeProject, services: &[String]) -> Result<Value> {
        let command = project.command(["ps", "--format", "json"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "list Docker Compose services",
            &project.name,
            &command,
            &output,
        )?;
        serde_json::from_slice(&output.stdout).map_err(Into::into)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeProject {
    pub(crate) name: String,
    pub(crate) project_directory: PathBuf,
    pub(crate) files: Vec<PathBuf>,
}

impl ComposeProject {
    fn command<const N: usize>(&self, args: [&str; N]) -> RuntimeCommand {
        let mut command = compose_cmd([])
            .arg("--project-name")
            .arg(&self.name)
            .arg("--project-directory")
            .arg(self.project_directory.display().to_string());
        for file in &self.files {
            command = command.arg("-f").arg(file.display().to_string());
        }
        command.args(args)
    }
}

fn compose_cmd<const N: usize>(args: [&str; N]) -> RuntimeCommand {
    RuntimeCommand::new("docker").arg("compose").args(args)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ComposeProject;

    #[test]
    fn compose_project_command_uses_docker_compose_plugin_argv() {
        let project = ComposeProject {
            name: "decune_project_abc123".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yml")],
        };

        let command = project.command(["config", "--format", "json"]);

        assert_eq!(command.program(), "docker");
        assert_eq!(command.args_vec()[0], "compose");
        assert!(command.args_vec().contains(&"--project-name".to_owned()));
        assert!(command.args_vec().contains(&"config".to_owned()));
    }
}

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::{
    config::{canonical::sha256_hex, hash::ComposeFileHashInput},
    error::ResultExt,
    runtime::command::{
        RuntimeCommand, RuntimeCommandRunner, RuntimeOutput, TokioRuntimeCommand, ensure_success,
    },
    workspace::Workspace,
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

    pub(crate) async fn config(&self, project: &ComposeCommandPlan) -> Result<Value> {
        let command = project.command(["config", "--format", "json"]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "read Docker Compose config",
            &project.project_name,
            &command,
            &output,
        )?;
        serde_json::from_slice(&output.stdout).map_err(Into::into)
    }

    pub(crate) async fn build(
        &self,
        project: &ComposeCommandPlan,
        services: &[String],
    ) -> Result<()> {
        let command = project.command(["build"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "build Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn up(&self, project: &ComposeCommandPlan, services: &[String]) -> Result<()> {
        let command = project.command(["up", "-d"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "start Docker Compose project",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn stop(
        &self,
        project: &ComposeCommandPlan,
        services: &[String],
    ) -> Result<()> {
        let command = project.command(["stop"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "stop Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn down(&self, project: &ComposeCommandPlan) -> Result<()> {
        let command = project.command(["down"]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "remove Docker Compose project",
            &project.project_name,
            &command,
            &output,
        )
    }

    pub(crate) async fn ps(
        &self,
        project: &ComposeCommandPlan,
        services: &[String],
    ) -> Result<Value> {
        let command = project.command(["ps", "--format", "json"]).args(services);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "list Docker Compose services",
            &project.project_name,
            &command,
            &output,
        )?;
        serde_json::from_slice(&output.stdout).map_err(Into::into)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeProjectPlan {
    project_name: String,
    pub(crate) project_directory: PathBuf,
    files: Vec<ComposeFilePlan>,
    generated_override_path: PathBuf,
    config_hash_files: Vec<ComposeFileHashInput>,
}

impl ComposeProjectPlan {
    pub(crate) fn resolve(
        workspace: &Workspace,
        devcontainer_dir: &Path,
        compose_files: &[String],
    ) -> Result<Self> {
        if compose_files.is_empty() {
            return Err(anyhow!("dockerComposeFile must not be empty"));
        }

        let files = compose_files
            .iter()
            .map(|file| resolve_compose_file(devcontainer_dir, file))
            .collect::<Result<Vec<_>>>()?;
        let first_file = files
            .first()
            .expect("compose files are checked as non-empty before resolution");
        let project_directory_path = first_file.resolved_path.parent().ok_or_else(|| {
            anyhow!(
                "Failed to resolve Docker Compose project directory from file: {}",
                first_file.resolved_path.display()
            )
        })?;
        let project_directory = project_directory_path.canonicalize().with_path_context(
            "canonicalize Docker Compose project directory",
            project_directory_path,
        )?;
        let config_hash_files = files
            .iter()
            .map(|file| ComposeFileHashInput {
                canonical_path: file.canonical_path.display().to_string(),
                digest: file.digest.clone(),
            })
            .collect();

        Ok(Self {
            project_name: compose_project_name(workspace),
            project_directory,
            files,
            generated_override_path: workspace.paths().state_dir().join("compose.override.yaml"),
            config_hash_files,
        })
    }

    pub(crate) fn project_name(&self) -> &str {
        &self.project_name
    }

    pub(crate) fn project_directory(&self) -> &Path {
        &self.project_directory
    }

    pub(crate) fn generated_override_path(&self) -> PathBuf {
        self.generated_override_path.clone()
    }

    pub(crate) fn config_hash_files(&self) -> &[ComposeFileHashInput] {
        &self.config_hash_files
    }

    pub(crate) fn command_plan_without_generated_override(&self) -> ComposeCommandPlan {
        ComposeCommandPlan {
            project_name: self.project_name.clone(),
            project_directory: self.project_directory.clone(),
            files: self
                .files
                .iter()
                .map(|file| file.canonical_path.clone())
                .collect(),
        }
    }

    pub(crate) fn command_plan_with_generated_override(&self) -> ComposeCommandPlan {
        let mut files = self
            .files
            .iter()
            .map(|file| file.canonical_path.clone())
            .collect::<Vec<_>>();
        files.push(self.generated_override_path.clone());

        ComposeCommandPlan {
            project_name: self.project_name.clone(),
            project_directory: self.project_directory.clone(),
            files,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeFilePlan {
    resolved_path: PathBuf,
    canonical_path: PathBuf,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCommandPlan {
    pub(crate) project_name: String,
    pub(crate) project_directory: PathBuf,
    pub(crate) files: Vec<PathBuf>,
}

impl ComposeCommandPlan {
    pub(crate) fn command<const N: usize>(&self, args: [&str; N]) -> RuntimeCommand {
        let mut command = compose_cmd([])
            .arg("--project-name")
            .arg(&self.project_name)
            .arg("--project-directory")
            .arg(self.project_directory.display().to_string());
        for file in &self.files {
            command = command.arg("-f").arg(file.display().to_string());
        }
        command.args(args)
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
        ComposeCommandPlan {
            project_name: self.name.clone(),
            project_directory: self.project_directory.clone(),
            files: self.files.clone(),
        }
        .command(args)
    }
}

fn compose_cmd<const N: usize>(args: [&str; N]) -> RuntimeCommand {
    RuntimeCommand::new("docker").arg("compose").args(args)
}

fn compose_project_name(workspace: &Workspace) -> String {
    format!("decune-{}-{}", workspace.safe_slug(), workspace.id())
}

fn resolve_compose_file(devcontainer_dir: &Path, value: &str) -> Result<ComposeFilePlan> {
    reject_unsupported_compose_file_reference(value)?;

    let path = Path::new(value);
    let resolved_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        devcontainer_dir.join(path)
    };
    let canonical_path = resolved_path
        .canonicalize()
        .with_path_context("canonicalize Docker Compose file", &resolved_path)?;
    let contents =
        fs::read(&canonical_path).with_path_context("read Docker Compose file", &canonical_path)?;

    Ok(ComposeFilePlan {
        resolved_path,
        canonical_path,
        digest: sha256_hex(&contents),
    })
}

fn reject_unsupported_compose_file_reference(value: &str) -> Result<()> {
    if value == "-" || value.contains("://") {
        return Err(anyhow!(
            "Unsupported dockerComposeFile reference: {value}. Only local file paths are supported"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::workspace::Workspace;

    use super::{ComposeCommandPlan, ComposeProject, ComposeProjectPlan};

    fn fixture_workspace(name: &str) -> (tempfile::TempDir, Workspace) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(name);
        fs::create_dir(&root).unwrap();
        (temp, Workspace::resolve(&root).unwrap())
    }

    fn write_compose_file(path: impl AsRef<std::path::Path>, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn compose_project_command_uses_docker_compose_plugin_argv() {
        let project = ComposeProject {
            name: "decune-project-abc123".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yml")],
        };

        let command = project.command(["config", "--format", "json"]);

        assert_eq!(command.program(), "docker");
        assert_eq!(command.args_vec()[0], "compose");
        assert!(command.args_vec().contains(&"--project-name".to_owned()));
        assert!(command.args_vec().contains(&"config".to_owned()));
    }

    #[test]
    fn compose_project_name_is_stable_for_same_workspace_path() {
        let (_temp, workspace) = fixture_workspace("Project Name");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

        let first =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();
        let second =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();

        assert_eq!(first.project_name(), second.project_name());
        assert_eq!(
            first.project_name(),
            format!("decune-project-name-{}", workspace.id())
        );
    }

    #[test]
    fn compose_project_name_includes_workspace_id_for_distinct_workspaces() {
        let (_first_temp, first_workspace) = fixture_workspace("Project Name");
        let (_second_temp, second_workspace) = fixture_workspace("Project Name");
        for workspace in [&first_workspace, &second_workspace] {
            let devcontainer_dir = workspace.root().join(".devcontainer");
            fs::create_dir(&devcontainer_dir).unwrap();
            write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        }

        let first = ComposeProjectPlan::resolve(
            &first_workspace,
            &first_workspace.root().join(".devcontainer"),
            &["compose.yaml".into()],
        )
        .unwrap();
        let second = ComposeProjectPlan::resolve(
            &second_workspace,
            &second_workspace.root().join(".devcontainer"),
            &["compose.yaml".into()],
        )
        .unwrap();

        assert_ne!(first_workspace.id(), second_workspace.id());
        assert_ne!(first.project_name(), second.project_name());
    }

    #[test]
    fn compose_plan_includes_explicit_project_name_flag() {
        let command_plan = ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
        };

        let command = command_plan.command(["config", "--format", "json"]);

        assert_eq!(
            command.args_vec(),
            &[
                "compose",
                "--project-name",
                "decune-project-abc123def456",
                "--project-directory",
                "/workspace",
                "-f",
                "/workspace/compose.yaml",
                "config",
                "--format",
                "json",
            ]
        );
        assert_eq!(command.env_value("COMPOSE_PROJECT_NAME"), None);
    }

    #[test]
    fn compose_plan_preserves_multi_file_order() {
        let (_temp, workspace) = fixture_workspace("multi-file");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        write_compose_file(
            devcontainer_dir.join("compose.override.yaml"),
            "services: {}\n",
        );

        let plan = ComposeProjectPlan::resolve(
            &workspace,
            &devcontainer_dir,
            &["compose.yaml".into(), "compose.override.yaml".into()],
        )
        .unwrap();
        let command = plan
            .command_plan_without_generated_override()
            .command(["config"]);

        let file_args = command
            .args_vec()
            .windows(2)
            .filter(|args| args[0] == "-f")
            .map(|args| args[1].clone())
            .collect::<Vec<_>>();

        assert_eq!(
            file_args,
            vec![
                devcontainer_dir
                    .join("compose.yaml")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                devcontainer_dir
                    .join("compose.override.yaml")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
            ]
        );
    }

    #[test]
    fn compose_project_directory_is_first_compose_file_parent() {
        let (_temp, workspace) = fixture_workspace("project-directory");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        let compose_dir = devcontainer_dir.join("compose");
        fs::create_dir_all(&compose_dir).unwrap();
        write_compose_file(compose_dir.join("compose.yaml"), "services: {}\n");
        write_compose_file(
            devcontainer_dir.join("compose.override.yaml"),
            "services: {}\n",
        );

        let plan = ComposeProjectPlan::resolve(
            &workspace,
            &devcontainer_dir,
            &[
                "compose/compose.yaml".into(),
                "compose.override.yaml".into(),
            ],
        )
        .unwrap();

        assert_eq!(
            plan.project_directory(),
            compose_dir.canonicalize().unwrap().as_path()
        );
    }

    #[cfg(unix)]
    #[test]
    fn compose_project_directory_uses_declared_first_compose_file_parent_for_symlink() {
        let (_temp, workspace) = fixture_workspace("symlink-project-directory");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        let target_dir = workspace.root().join("shared-compose");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        write_compose_file(target_dir.join("compose.yaml"), "services: {}\n");
        std::os::unix::fs::symlink(
            target_dir.join("compose.yaml"),
            devcontainer_dir.join("compose.yaml"),
        )
        .unwrap();

        let plan =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();

        assert_eq!(
            plan.project_directory(),
            devcontainer_dir.canonicalize().unwrap().as_path()
        );
        assert_eq!(
            plan.config_hash_files()[0].canonical_path,
            target_dir
                .join("compose.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[test]
    fn compose_generated_override_path_is_under_state_directory() {
        let (_temp, workspace) = fixture_workspace("generated-override");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

        let plan =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();

        assert_eq!(
            plan.generated_override_path(),
            workspace.paths().state_dir().join("compose.override.yaml")
        );
    }

    #[test]
    fn compose_project_plan_collects_canonical_file_hash_inputs() {
        let (_temp, workspace) = fixture_workspace("config-hash-input");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");

        let plan =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["compose.yaml".into()])
                .unwrap();
        let inputs = plan.config_hash_files();

        assert_eq!(inputs.len(), 1);
        assert_eq!(
            inputs[0].canonical_path,
            devcontainer_dir
                .join("compose.yaml")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(inputs[0].digest.len(), 64);
    }

    #[test]
    fn compose_project_plan_rejects_missing_compose_file() {
        let (_temp, workspace) = fixture_workspace("missing-compose-file");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();

        let error =
            ComposeProjectPlan::resolve(&workspace, &devcontainer_dir, &["missing.yaml".into()])
                .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Failed to canonicalize Docker Compose file"));
        assert!(message.contains("missing.yaml"));
    }
}

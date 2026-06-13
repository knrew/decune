#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer};

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

    pub(crate) async fn config_json(
        &self,
        project: &ComposeCommandPlan,
    ) -> Result<ComposeConfigModel> {
        let command = project.command(["config", "--format", "json"]);
        let output = self.runner.run_capture(command.clone()).await?;
        ensure_success(
            "read Docker Compose config",
            &project.project_name,
            &command,
            &output,
        )?;
        serde_json::from_slice(&output.stdout).map_err(|error| {
            anyhow!(
                "Failed to parse Docker Compose config JSON for project {} from files {}: {error}",
                project.project_name,
                project.file_list()
            )
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

    pub(crate) async fn up(
        &self,
        project: &ComposeCommandPlan,
        options: ComposeUpOptions,
        services: &[String],
    ) -> Result<()> {
        let command = compose_up_command(project, options, services);
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
        serde_json::from_slice(&output.stdout).map_err(|error| {
            anyhow!(
                "Failed to parse Docker Compose ps JSON for project {} service `{service}`: {error}",
                project.project_name
            )
        })
    }
}

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
        let model = self
            .cli
            .config_json(&project.command_plan_without_generated_override())
            .await?;
        model.validate_services(validation)?;
        Ok(model)
    }

    pub(crate) async fn config_model_with_generated_override(
        &self,
        project: &ComposeProjectPlan,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<ComposeConfigModel> {
        let model = self
            .cli
            .config_json(&project.command_plan_with_generated_override())
            .await?;
        model.validate_services(validation)?;
        Ok(model)
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposeConfigModel {
    #[serde(default)]
    services: std::collections::BTreeMap<String, ComposeConfigService>,
}

impl ComposeConfigModel {
    pub(crate) fn has_service(&self, service: &str) -> bool {
        self.services.contains_key(service)
    }

    pub(crate) fn service(&self, service: &str) -> Option<&ComposeConfigService> {
        self.services.get(service)
    }

    pub(crate) fn validate_services(
        &self,
        validation: &ComposeServiceValidation<'_>,
    ) -> Result<()> {
        validate_absolute_workspace_folder(validation.workspace_folder)?;
        if !self.has_service(validation.primary_service) {
            return Err(missing_compose_service_error(
                validation.project_name,
                "primary service",
                validation.primary_service,
            ));
        }

        if let Some(run_services) = validation.run_services {
            for service in run_services {
                if !self.has_service(service) {
                    return Err(missing_compose_service_error(
                        validation.project_name,
                        "runServices service",
                        service,
                    ));
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigService {
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default)]
    pub(crate) build: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposeServiceValidation<'a> {
    pub(crate) primary_service: &'a str,
    pub(crate) run_services: Option<&'a [String]>,
    pub(crate) workspace_folder: &'a str,
    pub(crate) project_name: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposePsContainer {
    #[serde(alias = "Id", rename = "ID")]
    pub(crate) id: String,
    #[serde(default, rename = "Name")]
    pub(crate) name: Option<String>,
    #[serde(rename = "Service")]
    pub(crate) service: String,
    #[serde(default, rename = "State")]
    pub(crate) state: Option<String>,
    #[serde(
        default,
        rename = "Publishers",
        deserialize_with = "deserialize_null_as_empty_vec"
    )]
    pub(crate) published_ports: Vec<ComposePublishedPort>,
}

fn deserialize_null_as_empty_vec<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ComposePublishedPort {
    #[serde(default, rename = "URL")]
    pub(crate) url: Option<String>,
    #[serde(default, rename = "TargetPort")]
    pub(crate) target_port: Option<u16>,
    #[serde(default, rename = "PublishedPort")]
    pub(crate) published_port: Option<u16>,
    #[serde(default, rename = "Protocol")]
    pub(crate) protocol: Option<String>,
}

pub(crate) fn resolve_compose_container(
    project_name: &str,
    service: &str,
    containers: Vec<ComposePsContainer>,
) -> Result<ComposePsContainer> {
    match containers.len() {
        0 => Err(anyhow!(
            "Docker Compose project {project_name} service `{service}` has no running container"
        )),
        1 => Ok(containers
            .into_iter()
            .next()
            .expect("container length checked before extraction")),
        count => Err(anyhow!(
            "Docker Compose project {project_name} service `{service}` has {count} containers; expected exactly one"
        )),
    }
}

fn missing_compose_service_error(project_name: &str, role: &str, service: &str) -> anyhow::Error {
    anyhow!(
        "Docker Compose project {project_name} does not contain {role} `{service}`. The service may be disabled by Compose profiles"
    )
}

fn validate_absolute_workspace_folder(workspace_folder: &str) -> Result<()> {
    if workspace_folder.starts_with('/') {
        return Ok(());
    }

    Err(anyhow!(
        "workspaceFolder must be an absolute container path: {workspace_folder}"
    ))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeBuildOptions {
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeUpOptions {
    pub(crate) force_recreate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeDownOptions {
    pub(crate) volumes: bool,
    pub(crate) remove_orphans: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeLifecyclePlan {
    pub(crate) project: ComposeCommandPlan,
    pub(crate) services: Vec<String>,
    pub(crate) cleanup: ComposeCleanupPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCleanupPlan {
    pub(crate) remove_project: bool,
    pub(crate) remove_volumes: bool,
    pub(crate) remove_state: bool,
    pub(crate) remove_generated_images: bool,
}

impl ComposeLifecyclePlan {
    pub(crate) fn up(
        project: ComposeCommandPlan,
        primary_service: &str,
        run_services: Option<&[String]>,
    ) -> Self {
        Self {
            project,
            services: compose_target_services(primary_service, run_services),
            cleanup: ComposeCleanupPlan::keep_all(),
        }
    }

    pub(crate) fn down(
        project: ComposeCommandPlan,
        primary_service: &str,
        run_services: Option<&[String]>,
    ) -> Self {
        Self {
            project,
            services: compose_target_services(primary_service, run_services),
            cleanup: ComposeCleanupPlan::keep_all(),
        }
    }

    pub(crate) fn clean(project: ComposeCommandPlan, images: bool) -> Self {
        Self {
            project,
            services: Vec::new(),
            cleanup: ComposeCleanupPlan {
                remove_project: true,
                remove_volumes: true,
                remove_state: true,
                remove_generated_images: images,
            },
        }
    }
}

impl ComposeCleanupPlan {
    fn keep_all() -> Self {
        Self {
            remove_project: false,
            remove_volumes: false,
            remove_state: false,
            remove_generated_images: false,
        }
    }
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

    fn file_list(&self) -> String {
        self.files
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
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

fn compose_build_command(
    project: &ComposeCommandPlan,
    options: ComposeBuildOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["build"]);
    if options.no_cache {
        command = command.arg("--no-cache");
    }
    if options.pull {
        command = command.arg("--pull");
    }
    command.args(services)
}

fn compose_up_command(
    project: &ComposeCommandPlan,
    options: ComposeUpOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["up", "-d"]);
    if options.force_recreate {
        command = command.arg("--force-recreate");
    }
    command.args(services)
}

fn compose_down_command(
    project: &ComposeCommandPlan,
    options: ComposeDownOptions,
) -> RuntimeCommand {
    let mut command = project.command(["down"]);
    if options.volumes {
        command = command.arg("--volumes");
    }
    if options.remove_orphans {
        command = command.arg("--remove-orphans");
    }
    command
}

fn compose_target_services(primary_service: &str, run_services: Option<&[String]>) -> Vec<String> {
    let Some(run_services) = run_services else {
        return Vec::new();
    };

    let mut services = Vec::with_capacity(run_services.len() + 1);
    services.push(primary_service.to_owned());
    for service in run_services {
        if !services.iter().any(|existing| existing == service) {
            services.push(service.clone());
        }
    }
    services
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

    use super::{
        ComposeBuildOptions, ComposeCommandPlan, ComposeConfigModel, ComposeDownOptions,
        ComposeIntrospector, ComposeLifecyclePlan, ComposeProject, ComposeProjectPlan,
        ComposeServiceValidation, ComposeUpOptions, DockerComposeCli, resolve_compose_container,
    };
    use crate::runtime::command::{FakeRuntimeCommand, RuntimeOutput};

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

    fn lifecycle_command_plan() -> ComposeCommandPlan {
        ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
        }
    }

    #[test]
    fn compose_lifecycle_up_without_run_services_targets_whole_project() {
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", None);
        let command =
            super::compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

        assert!(plan.services.is_empty());
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
                "up",
                "-d",
            ]
        );
    }

    #[test]
    fn compose_lifecycle_up_with_run_services_includes_primary_service_first() {
        let run_services = vec!["db".to_owned()];
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
        let command =
            super::compose_up_command(&plan.project, ComposeUpOptions::default(), &plan.services);

        assert_eq!(plan.services, ["app", "db"]);
        assert_eq!(
            command.args_vec().iter().rev().take(4).collect::<Vec<_>>(),
            vec!["db", "app", "-d", "up"]
        );
    }

    #[test]
    fn compose_lifecycle_rebuild_maps_no_cache_pull_and_force_recreate() {
        let run_services = vec!["db".to_owned()];
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
        let build = super::compose_build_command(
            &plan.project,
            ComposeBuildOptions {
                no_cache: true,
                pull: true,
            },
            &plan.services,
        );
        let up = super::compose_up_command(
            &plan.project,
            ComposeUpOptions {
                force_recreate: true,
            },
            &plan.services,
        );

        assert_eq!(
            build.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
            vec!["db", "app", "--pull", "--no-cache", "build"]
        );
        assert_eq!(
            up.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
            vec!["db", "app", "--force-recreate", "-d", "up"]
        );
    }

    #[test]
    fn compose_lifecycle_down_keeps_state_volumes_and_images() {
        let run_services = vec!["db".to_owned()];
        let plan = ComposeLifecyclePlan::down(lifecycle_command_plan(), "app", Some(&run_services));

        assert_eq!(plan.services, ["app", "db"]);
        assert!(!plan.cleanup.remove_project);
        assert!(!plan.cleanup.remove_volumes);
        assert!(!plan.cleanup.remove_state);
        assert!(!plan.cleanup.remove_generated_images);
    }

    #[test]
    fn compose_clean_down_removes_project_volumes_orphans_without_rmi() {
        let plan = ComposeLifecyclePlan::clean(lifecycle_command_plan(), false);
        let command = super::compose_down_command(
            &plan.project,
            ComposeDownOptions {
                volumes: plan.cleanup.remove_volumes,
                remove_orphans: true,
            },
        );

        assert!(plan.cleanup.remove_project);
        assert!(plan.cleanup.remove_state);
        assert!(!plan.cleanup.remove_generated_images);
        assert!(command.args_vec().contains(&"--volumes".to_owned()));
        assert!(command.args_vec().contains(&"--remove-orphans".to_owned()));
        assert!(!command.args_vec().contains(&"--rmi".to_owned()));
    }

    #[test]
    fn compose_clean_images_targets_only_decune_generated_image_policy() {
        let plan = ComposeLifecyclePlan::clean(lifecycle_command_plan(), true);

        assert!(plan.cleanup.remove_generated_images);
        assert!(plan.services.is_empty());
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

    #[test]
    fn compose_config_fixture_parses_services_without_rejecting_unknown_fields() {
        let model: ComposeConfigModel = serde_json::from_str(
            r#"
            {
              "name": "ignored",
              "services": {
                "app": {
                  "image": "alpine:3.20",
                  "working_dir": "/workspace",
                  "x-compose-version-dependent": true
                },
                "db": {
                  "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
              },
              "networks": {"default": {"name": "example_default"}}
            }
            "#,
        )
        .unwrap();

        assert!(model.has_service("app"));
        assert!(model.has_service("db"));
        assert_eq!(
            model
                .service("app")
                .and_then(|service| service.image.as_deref()),
            Some("alpine:3.20")
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_missing_primary_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 does not contain primary service `app`. The service may be disabled by Compose profiles"
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_missing_run_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
        let run_services = vec!["app".to_owned(), "db".to_owned()];
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: Some(&run_services),
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 does not contain runServices service `db`. The service may be disabled by Compose profiles"
        );
    }

    #[test]
    fn compose_introspection_validation_rejects_profile_disabled_primary_service() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"db":{"image":"postgres:16"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "/workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert!(error.to_string().contains("disabled by Compose profiles"));
    }

    #[test]
    fn compose_introspection_validation_rejects_relative_workspace_folder() {
        let model: ComposeConfigModel =
            serde_json::from_str(r#"{"services":{"app":{"image":"alpine:3.20"}}}"#).unwrap();
        let validation = ComposeServiceValidation {
            primary_service: "app",
            run_services: None,
            workspace_folder: "workspace",
            project_name: "decune-project-abc123",
        };

        let error = model.validate_services(&validation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder must be an absolute container path: workspace"
        );
    }

    #[test]
    fn compose_ps_fixture_resolves_single_container_id() {
        let containers = serde_json::from_str(
            r#"
            [
              {
                "ID": "abc123",
                "Name": "project-app-1",
                "Service": "app",
                "State": "running",
                "Publishers": [
                  {"URL": "127.0.0.1", "TargetPort": 3000, "PublishedPort": 3000, "Protocol": "tcp"}
                ]
              }
            ]
            "#,
        )
        .unwrap();

        let container =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

        assert_eq!(container.id, "abc123");
        assert_eq!(container.service, "app");
        assert_eq!(container.state.as_deref(), Some("running"));
        assert_eq!(container.published_ports.len(), 1);
    }

    #[test]
    fn compose_ps_fixture_treats_null_publishers_as_empty_ports() {
        let containers = serde_json::from_str(
            r#"
            [
              {
                "ID": "abc123",
                "Name": "project-app-1",
                "Service": "app",
                "State": "running",
                "Publishers": null
              }
            ]
            "#,
        )
        .unwrap();

        let container =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap();

        assert_eq!(container.id, "abc123");
        assert!(container.published_ports.is_empty());
    }

    #[test]
    fn compose_ps_resolution_rejects_zero_containers() {
        let containers = serde_json::from_str("[]").unwrap();

        let error =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 service `app` has no running container"
        );
    }

    #[test]
    fn compose_ps_resolution_rejects_multiple_containers() {
        let containers = serde_json::from_str(
            r#"
            [
              {"ID": "abc123", "Name": "project-app-1", "Service": "app"},
              {"ID": "def456", "Name": "project-app-2", "Service": "app"}
            ]
            "#,
        )
        .unwrap();

        let error =
            resolve_compose_container("decune-project-abc123", "app", containers).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Docker Compose project decune-project-abc123 service `app` has 2 containers; expected exactly one"
        );
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
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let config = runtime.block_on(cli.config_json(&command_plan)).unwrap();
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

    #[test]
    fn compose_introspection_reads_user_and_generated_config_paths() {
        let (_temp, workspace) = fixture_workspace("introspection-paths");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
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

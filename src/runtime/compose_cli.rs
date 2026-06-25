use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value as JsonValue;

use crate::{
    config::{canonical::sha256_hex, hash::ComposeFileHashInput, types::MountType},
    docker::mounts::{DockerMountSpec, MountBindOptions, MountVolumeOptions},
    error::ResultExt,
    runtime::{
        command::{RuntimeCommand, RuntimeCommandRunner, TokioRuntimeCommand, ensure_success},
        compose_ports::{
            ComposePortEntry, ComposePublishedPortPlanningInput, classify_compose_published_ports,
            compose_published_port_invalid_config_error, compose_published_port_planning_input,
        },
    },
    workspace::Workspace,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeCliCapabilities {
    pub(crate) version_short: Option<String>,
    pub(crate) config_format_json: bool,
    pub(crate) ps_format_json: bool,
    pub(crate) build_with_dependencies: bool,
    pub(crate) pull_policy_always: bool,
    pub(crate) pull_ignore_buildable: bool,
    pub(crate) pull_include_deps: bool,
    pub(crate) up_force_recreate: bool,
    pub(crate) up_remove_orphans: bool,
}

impl ComposeCliCapabilities {
    const COMPOSE_OVERRIDE_TAG_MIN_VERSION: (u64, u64, u64) = (2, 24, 4);

    pub(crate) fn from_help_outputs(
        version_short: Option<String>,
        config_help: &str,
        ps_help: &str,
        build_help: &str,
        pull_help: &str,
        up_help: &str,
    ) -> Self {
        Self {
            version_short,
            config_format_json: help_contains_option(config_help, "--format"),
            ps_format_json: help_contains_option(ps_help, "--format"),
            build_with_dependencies: help_contains_option(build_help, "--with-dependencies"),
            pull_policy_always: help_contains_option(pull_help, "--policy"),
            pull_ignore_buildable: help_contains_option(pull_help, "--ignore-buildable"),
            pull_include_deps: help_contains_option(pull_help, "--include-deps"),
            up_force_recreate: help_contains_option(up_help, "--force-recreate"),
            up_remove_orphans: help_contains_option(up_help, "--remove-orphans"),
        }
    }

    pub(crate) fn ensure_required(&self) -> Result<()> {
        let mut missing = Vec::new();
        if !self.config_format_json {
            missing
                .push("docker compose config --format json (config --help does not list --format)");
        }
        if !self.ps_format_json {
            missing.push("docker compose ps --format json (ps --help does not list --format)");
        }
        if !self.build_with_dependencies {
            missing.push(
                "docker compose build --with-dependencies (build --help does not list --with-dependencies)",
            );
        }
        if !self.pull_policy_always {
            missing
                .push("docker compose pull --policy always (pull --help does not list --policy)");
        }
        if !self.pull_ignore_buildable {
            missing.push(
                "docker compose pull --ignore-buildable (pull --help does not list --ignore-buildable)",
            );
        }
        if !self.pull_include_deps {
            missing.push(
                "docker compose pull --include-deps (pull --help does not list --include-deps)",
            );
        }
        if !self.up_force_recreate {
            missing.push(
                "docker compose up --force-recreate (up --help does not list --force-recreate)",
            );
        }
        if !self.up_remove_orphans {
            missing.push(
                "docker compose up --remove-orphans (up --help does not list --remove-orphans)",
            );
        }
        if missing.is_empty() {
            return Ok(());
        }

        bail!(
            "Docker Compose v2 plugin is missing required capabilities: {}. Update Docker Compose v2 plugin to a newer release.",
            missing.join("; ")
        )
    }

    pub(crate) fn ensure_compose_override_tag(&self) -> Result<()> {
        let Some(version) = self
            .version_short
            .as_deref()
            .and_then(parse_compose_version)
        else {
            bail!(
                "Compose published port relocation requires Docker Compose v2.24.4 or newer; failed to determine Docker Compose version"
            );
        };

        if version < Self::COMPOSE_OVERRIDE_TAG_MIN_VERSION {
            bail!(
                "Compose published port relocation requires Docker Compose v2.24.4 or newer; detected Docker Compose v{}.{}.{}",
                version.0,
                version.1,
                version.2
            );
        }

        Ok(())
    }
}

fn help_contains_option(help: &str, option: &str) -> bool {
    help.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, ',' | ';' | '[' | ']' | '(' | ')' | '{' | '}')
    })
    .any(|token| token == option || token.starts_with(&format!("{option}=")))
}

fn parse_compose_version(version: &str) -> Option<(u64, u64, u64)> {
    let trimmed = version.trim().trim_start_matches('v');
    let mut parts = trimmed.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeConfigOutput {
    pub(crate) model: ComposeConfigModel,
    pub(crate) canonical_model: JsonValue,
    pub(crate) published_port_entries: Vec<ComposePortEntry>,
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

    pub(crate) fn services(&self) -> impl Iterator<Item = (&String, &ComposeConfigService)> {
        self.services.iter()
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposePrimaryImage {
    pub(crate) base_image: String,
    pub(crate) has_build: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposePrimaryImageResolver<'a> {
    pub(crate) project_name: &'a str,
    pub(crate) service: &'a str,
}

impl ComposePrimaryImageResolver<'_> {
    pub(crate) fn resolve(self, model: &ComposeConfigModel) -> Result<ComposePrimaryImage> {
        let Some(service_model) = model.service(self.service) else {
            bail!(
                "Docker Compose project {} primary service `{}` is missing",
                self.project_name,
                self.service
            );
        };
        let has_build = service_model.build.is_some();
        if let Some(image) = service_model
            .image
            .as_ref()
            .filter(|image| !image.trim().is_empty())
        {
            return Ok(ComposePrimaryImage {
                base_image: image.clone(),
                has_build,
            });
        }
        if has_build {
            return Ok(ComposePrimaryImage {
                base_image: format!("{}-{}", self.project_name, self.service),
                has_build,
            });
        }

        bail!(
            "Docker Compose project {} primary service `{}` did not resolve an image or build",
            self.project_name,
            self.service
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigService {
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default)]
    pub(crate) build: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) working_dir: Option<String>,
    #[serde(default)]
    pub(crate) network_mode: Option<String>,
    #[serde(default)]
    pub(crate) scale: Option<u64>,
    #[serde(default)]
    pub(crate) deploy: ComposeConfigDeploy,
    #[serde(default, deserialize_with = "deserialize_compose_startup_value")]
    pub(crate) entrypoint: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_compose_startup_value")]
    pub(crate) command: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) ports: Vec<JsonValue>,
}

impl ComposeConfigService {
    pub(crate) fn effective_replica_count(&self) -> u64 {
        self.scale.or(self.deploy.replicas).unwrap_or(1)
    }

    pub(crate) fn uses_host_network(&self) -> bool {
        self.network_mode.as_deref() == Some("host")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ComposeConfigDeploy {
    #[serde(default)]
    pub(crate) replicas: Option<u64>,
}

fn deserialize_compose_startup_value<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<JsonValue>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(value) => {
            if value.is_empty() {
                Ok(Some(Vec::new()))
            } else {
                Ok(Some(vec![value]))
            }
        }
        JsonValue::Array(values) => values
            .into_iter()
            .map(|value| match value {
                JsonValue::String(value) => Ok(value),
                other => Err(de::Error::custom(format!(
                    "Docker Compose startup value must contain only strings: {other}"
                ))),
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some),
        other => Err(de::Error::custom(format!(
            "Docker Compose startup value must be null, string, or string array: {other}"
        ))),
    }
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

fn parse_compose_ps_json(
    stdout: &[u8],
    project_name: &str,
    service: &str,
) -> Result<Vec<ComposePsContainer>> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }

    match serde_json::from_slice::<JsonValue>(stdout) {
        Ok(JsonValue::Array(values)) => values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| compose_ps_parse_error(project_name, service, error)),
        Ok(JsonValue::Object(_)) => serde_json::from_slice(stdout)
            .map(|container| vec![container])
            .map_err(|error| compose_ps_parse_error(project_name, service, error)),
        Ok(other) => Err(anyhow!(
            "Failed to parse Docker Compose ps JSON for project {} service `{service}`: expected object or array, got {other}",
            project_name
        )),
        Err(first_error) => {
            let lines = String::from_utf8_lossy(stdout);
            let containers = lines
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str::<ComposePsContainer>)
                .collect::<std::result::Result<Vec<_>, _>>();
            containers.map_err(|line_error| {
                anyhow!(
                    "Failed to parse Docker Compose ps JSON for project {} service `{service}`: {first_error}; JSON Lines parse failed: {line_error}",
                    project_name
                )
            })
        }
    }
}

fn compose_ps_parse_error(
    project_name: &str,
    service: &str,
    error: serde_json::Error,
) -> anyhow::Error {
    anyhow!(
        "Failed to parse Docker Compose ps JSON for project {} service `{service}`: {error}",
        project_name
    )
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
    generated_override_env: BTreeMap<String, String>,
    generated_override_redactions: Vec<String>,
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
            generated_override_env: BTreeMap::new(),
            generated_override_redactions: Vec::new(),
        })
    }

    pub(crate) fn project_name(&self) -> &str {
        &self.project_name
    }

    #[cfg(test)]
    pub(crate) fn project_directory(&self) -> &Path {
        &self.project_directory
    }

    pub(crate) fn generated_override_path(&self) -> PathBuf {
        self.generated_override_path.clone()
    }

    pub(crate) fn config_hash_files(&self) -> &[ComposeFileHashInput] {
        &self.config_hash_files
    }

    pub(crate) fn with_generated_override_env(
        mut self,
        env: BTreeMap<String, String>,
        redactions: Vec<String>,
    ) -> Self {
        self.generated_override_env = env;
        self.generated_override_redactions = redactions;
        self
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
            env: BTreeMap::new(),
            redactions: Vec::new(),
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
            env: self.generated_override_env.clone(),
            redactions: self.generated_override_redactions.clone(),
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
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) redactions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ComposeOverridePatch {
    services: BTreeMap<String, ComposeOverrideServicePatch>,
    forbidden_secret_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOverrideServicePatch {
    name: String,
    image: Option<String>,
    pull_policy: Option<String>,
    labels: BTreeMap<String, String>,
    environment: BTreeMap<String, ComposeOverrideEnvironmentValue>,
    user: Option<String>,
    init: Option<bool>,
    privileged: Option<bool>,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    mounts: Vec<ComposeOverrideMount>,
    ports_override: Vec<ComposeOverridePortEntry>,
    entrypoint: Vec<String>,
    command: Vec<String>,
    forbidden_secret_values: Vec<String>,
}

pub(crate) type ComposeOverridePortEntry = BTreeMap<String, JsonValue>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposeOverrideEnvironmentValue {
    Literal(String),
    Interpolated {
        placeholder: String,
        redactions: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOverrideMount {
    source: Option<String>,
    target: String,
    mount_type: MountType,
    read_only: bool,
    consistency: Option<String>,
    bind_options: Option<MountBindOptions>,
    volume_options: Option<MountVolumeOptions>,
}

impl ComposeOverridePatch {
    pub(crate) fn new(primary: ComposeOverrideServicePatch) -> Self {
        let forbidden_secret_values = primary.forbidden_secret_values();
        Self {
            services: BTreeMap::from([(primary.name.clone(), primary)]),
            forbidden_secret_values,
        }
    }

    pub(crate) fn service(mut self, service: ComposeOverrideServicePatch) -> Self {
        self.forbidden_secret_values
            .extend(service.forbidden_secret_values());
        self.services.insert(service.name.clone(), service);
        self
    }

    pub(crate) fn to_yaml(&self) -> Result<String> {
        let mut content = String::new();
        content.push_str("services:\n");
        for (service_name, service) in &self.services {
            append_indent(&mut content, 2);
            content.push_str(&yaml_quote(service_name));
            content.push_str(":\n");
            service.append_yaml(&mut content);
        }
        self.ensure_no_forbidden_secret_values(&content)?;
        Ok(content)
    }

    fn ensure_no_forbidden_secret_values(&self, content: &str) -> Result<()> {
        for secret in self
            .forbidden_secret_values
            .iter()
            .filter(|secret| !secret.is_empty())
        {
            if content.contains(secret) {
                bail!("Generated Docker Compose override contains a forbidden secret value");
            }
        }
        Ok(())
    }
}

impl ComposeOverrideServicePatch {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: None,
            pull_policy: None,
            labels: BTreeMap::new(),
            environment: BTreeMap::new(),
            user: None,
            init: None,
            privileged: None,
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            mounts: Vec::new(),
            ports_override: Vec::new(),
            entrypoint: Vec::new(),
            command: Vec::new(),
            forbidden_secret_values: Vec::new(),
        }
    }

    pub(crate) fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub(crate) fn pull_policy_never(mut self) -> Self {
        self.pull_policy = Some("never".to_owned());
        self
    }

    #[cfg(test)]
    pub(crate) fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        if !key.starts_with("com.docker.compose.") {
            self.labels.insert(key, value.into());
        }
        self
    }

    pub(crate) fn labels(mut self, labels: &BTreeMap<String, String>) -> Self {
        for (key, value) in labels {
            if !key.starts_with("com.docker.compose.") {
                self.labels.insert(key.clone(), value.clone());
            }
        }
        self
    }

    pub(crate) fn environment(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(
            key.into(),
            ComposeOverrideEnvironmentValue::Literal(value.into()),
        );
        self
    }

    pub(crate) fn interpolated_environment(
        mut self,
        key: impl Into<String>,
        placeholder: impl Into<String>,
        redactions: Vec<String>,
    ) -> Self {
        self.environment.insert(
            key.into(),
            ComposeOverrideEnvironmentValue::Interpolated {
                placeholder: placeholder.into(),
                redactions,
            },
        );
        self
    }

    pub(crate) fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub(crate) fn init(mut self, init: bool) -> Self {
        self.init = Some(init);
        self
    }

    pub(crate) fn privileged(mut self, privileged: bool) -> Self {
        self.privileged = Some(privileged);
        self
    }

    pub(crate) fn cap_add(mut self, cap_add: &[String]) -> Self {
        self.cap_add.extend(cap_add.iter().cloned());
        self
    }

    pub(crate) fn security_opt(mut self, security_opt: &[String]) -> Self {
        self.security_opt.extend(security_opt.iter().cloned());
        self
    }

    pub(crate) fn mount(mut self, mount: ComposeOverrideMount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub(crate) fn mounts(mut self, mounts: &[DockerMountSpec]) -> Self {
        self.mounts
            .extend(mounts.iter().cloned().map(ComposeOverrideMount::from));
        self
    }

    pub(crate) fn ports_override(mut self, ports: Vec<ComposeOverridePortEntry>) -> Self {
        self.ports_override = ports;
        self
    }

    pub(crate) fn entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = entrypoint;
        self
    }

    pub(crate) fn command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    #[cfg(test)]
    pub(crate) fn keepalive_command(mut self, enabled: bool) -> Self {
        if enabled {
            self.command = vec!["sleep".to_owned(), "infinity".to_owned()];
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn secret_value_forbidden(mut self, value: impl Into<String>) -> Self {
        self.forbidden_secret_values.push(value.into());
        self
    }

    fn forbidden_secret_values(&self) -> Vec<String> {
        let mut values = self.forbidden_secret_values.clone();
        for value in self.environment.values() {
            if let ComposeOverrideEnvironmentValue::Interpolated { redactions, .. } = value {
                values.extend(redactions.clone());
            }
        }
        values
    }

    fn append_yaml(&self, content: &mut String) {
        if let Some(image) = &self.image {
            append_yaml_scalar(content, 4, "image", image);
        }
        if let Some(pull_policy) = &self.pull_policy {
            append_yaml_scalar(content, 4, "pull_policy", pull_policy);
        }
        append_yaml_map(content, 4, "labels", &self.labels);
        append_yaml_environment(content, 4, &self.environment);
        if let Some(user) = &self.user {
            append_yaml_scalar(content, 4, "user", user);
        }
        if let Some(init) = self.init {
            append_yaml_bool(content, 4, "init", init);
        }
        if let Some(privileged) = self.privileged {
            append_yaml_bool(content, 4, "privileged", privileged);
        }
        append_yaml_string_list(content, 4, "cap_add", &self.cap_add);
        append_yaml_string_list(content, 4, "security_opt", &self.security_opt);
        append_yaml_mounts(content, 4, &self.mounts);
        append_yaml_ports_override(content, 4, &self.ports_override);
        append_yaml_string_list(content, 4, "entrypoint", &self.entrypoint);
        append_yaml_string_list(content, 4, "command", &self.command);
    }
}

impl ComposeOverrideMount {
    #[cfg(test)]
    pub(crate) fn bind(
        source: impl Into<String>,
        target: impl Into<String>,
        read_only: bool,
    ) -> Self {
        Self {
            source: Some(source.into()),
            target: target.into(),
            mount_type: MountType::Bind,
            read_only,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }
    }
}

impl From<DockerMountSpec> for ComposeOverrideMount {
    fn from(mount: DockerMountSpec) -> Self {
        Self {
            source: mount.source,
            target: mount.target,
            mount_type: mount.mount_type,
            read_only: mount.read_only,
            consistency: mount.consistency,
            bind_options: mount.bind_options,
            volume_options: mount.volume_options,
        }
    }
}

pub(crate) fn write_compose_override(path: &Path, patch: &ComposeOverridePatch) -> Result<()> {
    let content = patch.to_yaml()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create Docker Compose generated override directory: {}",
                parent.display()
            )
        })?;
    }
    let temporary_path = path.with_extension("yaml.tmp");
    fs::write(&temporary_path, content).with_context(|| {
        format!(
            "Failed to write temporary Docker Compose generated override file: {}",
            temporary_path.display()
        )
    })?;
    fs::rename(&temporary_path, path).with_context(|| {
        format!(
            "Failed to replace Docker Compose generated override file: {}",
            path.display()
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeBuildOptions {
    pub(crate) with_dependencies: bool,
    pub(crate) no_cache: bool,
    pub(crate) pull: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposePullOptions {
    pub(crate) always: bool,
    pub(crate) ignore_buildable: bool,
    pub(crate) include_deps: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeUpOptions {
    pub(crate) force_recreate: bool,
    pub(crate) remove_orphans: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ComposeStopOptions {
    pub(crate) timeout_seconds: Option<i32>,
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

    pub(crate) fn down(project: ComposeCommandPlan) -> Self {
        Self {
            project,
            services: Vec::new(),
            cleanup: ComposeCleanupPlan::keep_all(),
        }
    }

    pub(crate) fn remove(project: ComposeCommandPlan, images: bool) -> Self {
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
            .current_dir(self.project_directory.clone())
            .arg("--project-name")
            .arg(&self.project_name)
            .arg("--project-directory")
            .arg(self.project_directory.display().to_string());
        for (key, value) in &self.env {
            command = command.env(key.clone(), value.clone());
        }
        command = command.redact_values(self.redactions.clone());
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

fn compose_cmd<const N: usize>(args: [&str; N]) -> RuntimeCommand {
    RuntimeCommand::new("docker").arg("compose").args(args)
}

fn compose_config_command(project: &ComposeCommandPlan, services: &[String]) -> RuntimeCommand {
    project
        .command(["config", "--format", "json"])
        .args(services)
}

fn compose_build_command(
    project: &ComposeCommandPlan,
    options: ComposeBuildOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["build"]);
    if options.with_dependencies {
        command = command.arg("--with-dependencies");
    }
    if options.no_cache {
        command = command.arg("--no-cache");
    }
    if options.pull {
        command = command.arg("--pull");
    }
    command.args(services)
}

fn compose_pull_command(
    project: &ComposeCommandPlan,
    options: ComposePullOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["pull"]);
    if options.ignore_buildable {
        command = command.arg("--ignore-buildable");
    }
    if options.include_deps {
        command = command.arg("--include-deps");
    }
    if options.always {
        command = command.arg("--policy").arg("always");
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
    if options.remove_orphans {
        command = command.arg("--remove-orphans");
    }
    command.args(services)
}

fn compose_stop_command(
    project: &ComposeCommandPlan,
    options: ComposeStopOptions,
    services: &[String],
) -> RuntimeCommand {
    let mut command = project.command(["stop"]);
    if let Some(timeout_seconds) = options.timeout_seconds {
        command = command.arg("--timeout").arg(timeout_seconds.to_string());
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

fn append_yaml_scalar(content: &mut String, indent: usize, key: &str, value: &str) {
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(": ");
    content.push_str(&yaml_quote(value));
    content.push('\n');
}

fn append_yaml_bool(content: &mut String, indent: usize, key: &str, value: bool) {
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(": ");
    content.push_str(if value { "true" } else { "false" });
    content.push('\n');
}

fn append_yaml_map(
    content: &mut String,
    indent: usize,
    key: &str,
    values: &BTreeMap<String, String>,
) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(":\n");
    for (name, value) in values {
        append_indent(content, indent + 2);
        content.push_str(&yaml_quote(name));
        content.push_str(": ");
        content.push_str(&yaml_quote(value));
        content.push('\n');
    }
}

fn append_yaml_environment(
    content: &mut String,
    indent: usize,
    values: &BTreeMap<String, ComposeOverrideEnvironmentValue>,
) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("environment:\n");
    for (name, value) in values {
        append_indent(content, indent + 2);
        content.push_str(&yaml_quote(name));
        content.push_str(": ");
        match value {
            ComposeOverrideEnvironmentValue::Literal(value) => {
                content.push_str(&yaml_quote(value));
            }
            ComposeOverrideEnvironmentValue::Interpolated { placeholder, .. } => {
                content.push_str(&yaml_quote(&format!("${{{placeholder}}}")));
            }
        }
        content.push('\n');
    }
}

fn append_yaml_string_list(content: &mut String, indent: usize, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str(key);
    content.push_str(":\n");
    for value in values {
        append_indent(content, indent + 2);
        content.push_str("- ");
        content.push_str(&yaml_quote(value));
        content.push('\n');
    }
}

fn append_yaml_mounts(content: &mut String, indent: usize, mounts: &[ComposeOverrideMount]) {
    if mounts.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("volumes:\n");
    for mount in mounts {
        append_indent(content, indent + 2);
        content.push_str("- type: ");
        content.push_str(match mount.mount_type {
            MountType::Bind => "bind",
            MountType::Volume => "volume",
            MountType::Tmpfs => "tmpfs",
        });
        content.push('\n');
        if let Some(source) = &mount.source {
            append_yaml_scalar(content, indent + 4, "source", source);
        }
        append_yaml_scalar(content, indent + 4, "target", &mount.target);
        if mount.read_only {
            append_yaml_bool(content, indent + 4, "read_only", true);
        }
        if let Some(consistency) = &mount.consistency {
            append_yaml_scalar(content, indent + 4, "consistency", consistency);
        }
        match mount.mount_type {
            MountType::Bind => append_yaml_bind_mount_options(content, indent + 4, mount),
            MountType::Volume => append_yaml_volume_mount_options(content, indent + 4, mount),
            MountType::Tmpfs => {}
        }
    }
}

fn append_yaml_ports_override(
    content: &mut String,
    indent: usize,
    ports: &[ComposeOverridePortEntry],
) {
    if ports.is_empty() {
        return;
    }
    append_indent(content, indent);
    content.push_str("ports: !override\n");
    for port in ports {
        append_indent(content, indent + 2);
        content.push_str("- ");
        append_yaml_object_fields_after_prefix(content, indent + 2, port);
    }
}

fn append_yaml_object_fields_after_prefix(
    content: &mut String,
    indent: usize,
    values: &BTreeMap<String, JsonValue>,
) {
    if values.is_empty() {
        content.push_str("{}\n");
        return;
    }

    let mut fields = values.iter();
    let Some((first_key, first_value)) = fields.next() else {
        unreachable!("empty object handled above");
    };
    content.push_str(first_key);
    content.push_str(": ");
    append_yaml_json_value(content, indent + 2, first_value);
    content.push('\n');

    for (key, value) in fields {
        append_indent(content, indent + 2);
        content.push_str(key);
        content.push_str(": ");
        append_yaml_json_value(content, indent + 2, value);
        content.push('\n');
    }
}

fn append_yaml_json_value(content: &mut String, indent: usize, value: &JsonValue) {
    match value {
        JsonValue::Null => content.push_str("null"),
        JsonValue::Bool(value) => content.push_str(if *value { "true" } else { "false" }),
        JsonValue::Number(value) => content.push_str(&value.to_string()),
        JsonValue::String(value) => content.push_str(&yaml_quote(value)),
        JsonValue::Array(values) => {
            if values.is_empty() {
                content.push_str("[]");
                return;
            }
            content.push('\n');
            for value in values {
                append_indent(content, indent);
                content.push_str("- ");
                append_yaml_json_value(content, indent + 2, value);
                content.push('\n');
            }
        }
        JsonValue::Object(values) => {
            if values.is_empty() {
                content.push_str("{}");
                return;
            }
            content.push('\n');
            for (key, value) in values {
                append_indent(content, indent);
                content.push_str(key);
                content.push_str(": ");
                append_yaml_json_value(content, indent + 2, value);
                content.push('\n');
            }
        }
    }
}

fn append_yaml_bind_mount_options(
    content: &mut String,
    indent: usize,
    mount: &ComposeOverrideMount,
) {
    append_indent(content, indent);
    content.push_str("bind:\n");
    if let Some(propagation) = mount
        .bind_options
        .as_ref()
        .and_then(|options| options.propagation)
    {
        append_yaml_scalar(content, indent + 2, "propagation", propagation.as_str());
    }
    let create_host_path = mount
        .bind_options
        .as_ref()
        .and_then(|options| options.create_mountpoint)
        .unwrap_or(false);
    append_yaml_bool(content, indent + 2, "create_host_path", create_host_path);
}

fn append_yaml_volume_mount_options(
    content: &mut String,
    indent: usize,
    mount: &ComposeOverrideMount,
) {
    let Some(volume_options) = &mount.volume_options else {
        return;
    };
    if volume_options.no_copy.is_none() && volume_options.subpath.is_none() {
        return;
    }

    append_indent(content, indent);
    content.push_str("volume:\n");
    if let Some(no_copy) = volume_options.no_copy {
        append_yaml_bool(content, indent + 2, "nocopy", no_copy);
    }
    if let Some(subpath) = &volume_options.subpath {
        append_yaml_scalar(content, indent + 2, "subpath", subpath);
    }
}

fn append_indent(content: &mut String, indent: usize) {
    for _ in 0..indent {
        content.push(' ');
    }
}

fn yaml_quote(value: &str) -> String {
    if value
        .chars()
        .any(|ch| matches!(ch, '\n' | '\r') || ch.is_control())
    {
        return yaml_double_quote(value);
    }

    format!("'{}'", value.replace('\'', "''"))
}

fn yaml_double_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{08}' => quoted.push_str("\\b"),
            '\u{0c}' => quoted.push_str("\\f"),
            ch if ch.is_control() => {
                quoted.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    };

    use crate::workspace::Workspace;

    use super::{
        ComposeBuildOptions, ComposeCliCapabilities, ComposeCommandPlan, ComposeConfigModel,
        ComposeConfigService, ComposeDownOptions, ComposeIntrospector, ComposeLifecyclePlan,
        ComposeOverrideMount, ComposeOverridePatch, ComposeOverrideServicePatch,
        ComposePrimaryImageResolver, ComposeProjectPlan, ComposePullOptions,
        ComposeServiceValidation, ComposeStopOptions, ComposeUpOptions, DockerComposeCli,
        parse_compose_ps_json, resolve_compose_container, write_compose_override,
    };
    use crate::runtime::command::{FakeRuntimeCommand, RuntimeOutput};
    use crate::runtime::compose_ports::ComposePortEligibility;

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
        let project = ComposeCommandPlan {
            project_name: "decune-project-abc123".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yml")],
            env: BTreeMap::new(),
            redactions: Vec::new(),
        };

        let command = project.command(["config", "--format", "json"]);

        assert_eq!(command.program(), "docker");
        assert_eq!(command.args_vec()[0], "compose");
        assert_eq!(command.current_dir_path(), Some(Path::new("/workspace")));
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
    fn compose_ps_json_accepts_single_object_output() {
        let containers = parse_compose_ps_json(
            br#"{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":null}"#,
            "decune-project-abc123",
            "app",
        )
        .unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "app-id");
        assert_eq!(containers[0].service, "app");
        assert!(containers[0].published_ports.is_empty());
    }

    #[test]
    fn compose_ps_json_accepts_array_output() {
        let containers = parse_compose_ps_json(
            br#"[{"ID":"app-id","Name":"project-app-1","Service":"app","State":"running","Publishers":[]}]"#,
            "decune-project-abc123",
            "app",
        )
        .unwrap();

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "app-id");
    }

    #[test]
    fn compose_ps_json_accepts_json_lines_output() {
        let containers = parse_compose_ps_json(
            b"{\"ID\":\"app-id\",\"Name\":\"project-app-1\",\"Service\":\"app\",\"State\":\"running\",\"Publishers\":[]}\n{\"ID\":\"sidecar-id\",\"Name\":\"project-sidecar-1\",\"Service\":\"sidecar\",\"State\":\"running\",\"Publishers\":[]}\n",
            "decune-project-abc123",
            "app",
        )
        .unwrap();

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "app-id");
        assert_eq!(containers[1].service, "sidecar");
    }

    #[test]
    fn compose_plan_includes_explicit_project_name_flag() {
        let command_plan = ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
            env: BTreeMap::new(),
            redactions: Vec::new(),
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
    fn compose_plan_passes_generated_override_env_as_child_env() {
        let command_plan = ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
            env: BTreeMap::from([(
                "DECUNE_CONTAINER_ENV_NPM_TOKEN".to_owned(),
                "secret-token".to_owned(),
            )]),
            redactions: vec!["secret-token".to_owned()],
        };

        let command = command_plan.command(["up", "-d"]);

        assert_eq!(
            command
                .env_value("DECUNE_CONTAINER_ENV_NPM_TOKEN")
                .map(String::as_str),
            Some("secret-token")
        );
        assert!(
            !command
                .args_vec()
                .iter()
                .any(|arg| arg.contains("secret-token"))
        );
        assert!(!command.sanitized_display().contains("secret-token"));
    }

    fn lifecycle_command_plan() -> ComposeCommandPlan {
        ComposeCommandPlan {
            project_name: "decune-project-abc123def456".to_owned(),
            project_directory: PathBuf::from("/workspace"),
            files: vec![PathBuf::from("/workspace/compose.yaml")],
            env: BTreeMap::new(),
            redactions: Vec::new(),
        }
    }

    fn runtime_output(stdout: impl AsRef<[u8]>) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.as_ref().to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }

    fn runtime_error_output(stderr: impl AsRef<[u8]>) -> RuntimeOutput {
        RuntimeOutput {
            stdout: Vec::new(),
            stderr: stderr.as_ref().to_vec(),
            exit_code: 1,
        }
    }

    fn valid_compose_capabilities() -> ComposeCliCapabilities {
        ComposeCliCapabilities::from_help_outputs(
            Some("2.40.0".to_owned()),
            "Usage: docker compose config [OPTIONS]\n      --format string",
            "Usage: docker compose ps [OPTIONS]\n      --format string",
            "Usage: docker compose build [OPTIONS]\n      --with-dependencies --no-cache --pull",
            "Usage: docker compose pull [OPTIONS]\n      --policy string --ignore-buildable --include-deps",
            "Usage: docker compose up [OPTIONS]\n      --force-recreate --remove-orphans",
        )
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
    fn compose_config_model_preserves_service_user() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "alpine:3.20",
                    "user": "1001:1002"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            model
                .service("app")
                .and_then(|service| service.user.as_deref()),
            Some("1001:1002")
        );
    }

    #[test]
    fn compose_config_model_preserves_port_policy_service_context() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "scaled": {
                    "image": "alpine:3.20",
                    "scale": 2
                },
                "deployed": {
                    "image": "alpine:3.20",
                    "deploy": {"replicas": 3}
                },
                "hostnet": {
                    "image": "alpine:3.20",
                    "network_mode": "host"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            model.service("scaled").unwrap().effective_replica_count(),
            2
        );
        assert_eq!(
            model.service("deployed").unwrap().effective_replica_count(),
            3
        );
        assert!(model.service("hostnet").unwrap().uses_host_network());
    }

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
    fn compose_introspector_builds_active_published_port_planning_input() {
        let (_temp, workspace) = fixture_workspace("active-port-planning");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
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
    fn compose_primary_image_resolver_uses_service_image_without_build() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "example/app:dev"
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "example/app:dev");
        assert!(!image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_uses_compose_build_default_tag_without_image() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "decune-project-abc123def456-app");
        assert!(image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_uses_canonical_image_when_build_is_tagged() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {
                    "image": "example/app:dev",
                    "build": {"context": ".", "dockerfile": "Dockerfile"}
                }
            }
        }))
        .unwrap();

        let image = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap();

        assert_eq!(image.base_image, "example/app:dev");
        assert!(image.has_build);
    }

    #[test]
    fn compose_primary_image_resolver_rejects_service_without_image_or_build() {
        let model: ComposeConfigModel = serde_json::from_value(serde_json::json!({
            "services": {
                "app": {}
            }
        }))
        .unwrap();

        let error = ComposePrimaryImageResolver {
            project_name: "decune-project-abc123def456",
            service: "app",
        }
        .resolve(&model)
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("did not resolve an image or build")
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
    fn compose_build_command_with_dependencies_combines_no_cache_and_pull() {
        let services = vec!["app".to_owned()];
        let command = super::compose_build_command(
            &lifecycle_command_plan(),
            ComposeBuildOptions {
                with_dependencies: true,
                no_cache: true,
                pull: true,
            },
            &services,
        );

        assert_eq!(
            command.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
            vec![
                "app",
                "--pull",
                "--no-cache",
                "--with-dependencies",
                "build"
            ]
        );
    }

    #[test]
    fn compose_lifecycle_rebuild_maps_no_cache_pull_and_force_recreate() {
        let run_services = vec!["db".to_owned()];
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
        let build = super::compose_build_command(
            &plan.project,
            ComposeBuildOptions {
                with_dependencies: true,
                no_cache: true,
                pull: true,
            },
            &plan.services,
        );
        let up = super::compose_up_command(
            &plan.project,
            ComposeUpOptions {
                force_recreate: true,
                remove_orphans: false,
            },
            &plan.services,
        );

        assert_eq!(
            build.args_vec().iter().rev().take(6).collect::<Vec<_>>(),
            vec![
                "db",
                "app",
                "--pull",
                "--no-cache",
                "--with-dependencies",
                "build"
            ]
        );
        assert_eq!(
            up.args_vec().iter().rev().take(5).collect::<Vec<_>>(),
            vec!["db", "app", "--force-recreate", "-d", "up"]
        );
    }

    #[test]
    fn compose_up_command_can_remove_orphans() {
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", None);
        let up = super::compose_up_command(
            &plan.project,
            ComposeUpOptions {
                force_recreate: true,
                remove_orphans: true,
            },
            &plan.services,
        );

        assert!(up.args_vec().contains(&"--force-recreate".to_owned()));
        assert!(up.args_vec().contains(&"--remove-orphans".to_owned()));
    }

    #[test]
    fn compose_pull_command_updates_image_only_services() {
        let run_services = vec!["db".to_owned()];
        let plan = ComposeLifecyclePlan::up(lifecycle_command_plan(), "app", Some(&run_services));
        let pull = super::compose_pull_command(
            &plan.project,
            ComposePullOptions {
                always: true,
                ignore_buildable: true,
                include_deps: true,
            },
            &plan.services,
        );

        assert_eq!(
            pull.args_vec().iter().rev().take(7).collect::<Vec<_>>(),
            vec![
                "db",
                "app",
                "always",
                "--policy",
                "--include-deps",
                "--ignore-buildable",
                "pull"
            ]
        );
    }

    #[test]
    fn compose_capability_valid_help_output_detects_required_options() {
        let capabilities = valid_compose_capabilities();

        assert_eq!(capabilities.version_short.as_deref(), Some("2.40.0"));
        assert!(capabilities.config_format_json);
        assert!(capabilities.ps_format_json);
        assert!(capabilities.build_with_dependencies);
        assert!(capabilities.pull_policy_always);
        assert!(capabilities.pull_ignore_buildable);
        assert!(capabilities.pull_include_deps);
        assert!(capabilities.up_force_recreate);
        assert!(capabilities.up_remove_orphans);
        capabilities.ensure_required().unwrap();
        capabilities.ensure_compose_override_tag().unwrap();
    }

    #[test]
    fn compose_capability_accepts_override_tag_minimum_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: Some("v2.24.4".to_owned()),
            ..valid_compose_capabilities()
        };

        capabilities.ensure_compose_override_tag().unwrap();
    }

    #[test]
    fn compose_capability_rejects_old_override_tag_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: Some("2.24.3".to_owned()),
            ..valid_compose_capabilities()
        };

        let error = capabilities
            .ensure_compose_override_tag()
            .unwrap_err()
            .to_string();

        assert!(error.contains(
            "Compose published port relocation requires Docker Compose v2.24.4 or newer"
        ));
        assert!(error.contains("detected Docker Compose v2.24.3"));
    }

    #[test]
    fn compose_capability_rejects_unknown_override_tag_version() {
        let capabilities = ComposeCliCapabilities {
            version_short: None,
            ..valid_compose_capabilities()
        };

        let error = capabilities
            .ensure_compose_override_tag()
            .unwrap_err()
            .to_string();

        assert!(error.contains(
            "Compose published port relocation requires Docker Compose v2.24.4 or newer"
        ));
        assert!(error.contains("failed to determine Docker Compose version"));
    }

    #[test]
    fn compose_capability_missing_build_with_dependencies_errors_clearly() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--no-cache --pull",
            "--policy string --ignore-buildable --include-deps",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose build --with-dependencies"));
        assert!(error.contains("build --help does not list --with-dependencies"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
    }

    #[test]
    fn compose_capability_missing_pull_include_deps_errors_clearly() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose pull --include-deps"));
        assert!(error.contains("pull --help does not list --include-deps"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
    }

    #[test]
    fn compose_capability_missing_config_format_mentions_config_format_json() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--services",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable --include-deps",
            "--force-recreate --remove-orphans",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose config --format json"));
        assert!(error.contains("config --help does not list --format"));
    }

    #[test]
    fn compose_capability_missing_up_options_prompts_compose_plugin_update() {
        let capabilities = ComposeCliCapabilities::from_help_outputs(
            Some("2.3.0".to_owned()),
            "--format string",
            "--format string",
            "--with-dependencies",
            "--policy string --ignore-buildable --include-deps",
            "--detach",
        );

        let error = capabilities.ensure_required().unwrap_err().to_string();

        assert!(error.contains("docker compose up --force-recreate"));
        assert!(error.contains("docker compose up --remove-orphans"));
        assert!(error.contains("Update Docker Compose v2 plugin"));
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

        assert!(capabilities.build_with_dependencies);
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
    fn compose_lifecycle_down_stops_whole_project_and_keeps_state_volumes_and_images() {
        let plan = ComposeLifecyclePlan::down(lifecycle_command_plan());
        let command = plan.project.command(["stop"]).args(&plan.services);

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
                "stop",
            ]
        );
        assert!(!plan.cleanup.remove_project);
        assert!(!plan.cleanup.remove_volumes);
        assert!(!plan.cleanup.remove_state);
        assert!(!plan.cleanup.remove_generated_images);
    }

    #[test]
    fn compose_stop_command_includes_timeout_when_requested() {
        let plan = ComposeLifecyclePlan::down(lifecycle_command_plan());
        let command = super::compose_stop_command(
            &plan.project,
            ComposeStopOptions {
                timeout_seconds: Some(37),
            },
            &plan.services,
        );

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
                "stop",
                "--timeout",
                "37",
            ]
        );
    }

    #[test]
    fn compose_remove_down_removes_project_volumes_orphans_without_rmi() {
        let plan = ComposeLifecyclePlan::remove(lifecycle_command_plan(), false);
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
    fn compose_remove_images_targets_only_decune_generated_image_policy() {
        let plan = ComposeLifecyclePlan::remove(lifecycle_command_plan(), true);

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
    fn compose_override_yaml_patches_only_primary_service() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .label("decune.managed", "true")
                .label("decune.workspace_id", "workspace-id")
                .environment("APP_ENV", "development")
                .user("decune")
                .mount(ComposeOverrideMount::bind(
                    "/host/cache",
                    "/workspaces/cache",
                    true,
                )),
        );

        let yaml = patch.to_yaml().unwrap();

        assert_eq!(
            yaml,
            concat!(
                "services:\n",
                "  'app':\n",
                "    labels:\n",
                "      'decune.managed': 'true'\n",
                "      'decune.workspace_id': 'workspace-id'\n",
                "    environment:\n",
                "      'APP_ENV': 'development'\n",
                "    user: 'decune'\n",
                "    volumes:\n",
                "      - type: bind\n",
                "        source: '/host/cache'\n",
                "        target: '/workspaces/cache'\n",
                "        read_only: true\n",
                "        bind:\n",
                "          create_host_path: false\n",
            )
        );
        assert!(!yaml.contains("sidecar"));
    }

    #[test]
    fn compose_override_yaml_sets_generated_image_and_pull_policy_never() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .image("decune/workspace:hash123")
                .pull_policy_never(),
        );

        let yaml = patch.to_yaml().unwrap();

        assert!(yaml.contains("    image: 'decune/workspace:hash123'\n"));
        assert!(yaml.contains("    pull_policy: 'never'\n"));
    }

    #[test]
    fn compose_override_yaml_replaces_ports_with_override_tag() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").ports_override(vec![
                BTreeMap::from([
                    ("app_protocol".to_owned(), serde_json::json!("http")),
                    ("host_ip".to_owned(), serde_json::json!("127.0.0.1")),
                    ("mode".to_owned(), serde_json::json!("host")),
                    ("name".to_owned(), serde_json::json!("web")),
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("published".to_owned(), serde_json::json!("3001")),
                    ("target".to_owned(), serde_json::json!(3000)),
                ]),
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("udp")),
                    ("published".to_owned(), serde_json::json!("8125")),
                    ("target".to_owned(), serde_json::json!(8125)),
                ]),
                BTreeMap::from([
                    ("protocol".to_owned(), serde_json::json!("tcp")),
                    ("target".to_owned(), serde_json::json!(9000)),
                ]),
            ]),
        );

        let yaml = patch.to_yaml().unwrap();

        assert_eq!(
            yaml,
            concat!(
                "services:\n",
                "  'app':\n",
                "    ports: !override\n",
                "      - app_protocol: 'http'\n",
                "        host_ip: '127.0.0.1'\n",
                "        mode: 'host'\n",
                "        name: 'web'\n",
                "        protocol: 'tcp'\n",
                "        published: '3001'\n",
                "        target: 3000\n",
                "      - protocol: 'udp'\n",
                "        published: '8125'\n",
                "        target: 8125\n",
                "      - protocol: 'tcp'\n",
                "        target: 9000\n",
            )
        );
    }

    #[test]
    fn compose_override_command_is_emitted_only_when_requested() {
        let keepalive = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").keepalive_command(true),
        )
        .to_yaml()
        .unwrap();
        let original = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").keepalive_command(false),
        )
        .to_yaml()
        .unwrap();

        assert!(keepalive.contains("    command:\n      - 'sleep'\n      - 'infinity'\n"));
        assert!(!original.contains("command:"));
    }

    #[test]
    fn compose_override_secret_leak_regression_does_not_persist_secret_literals() {
        let temp = tempfile::tempdir().unwrap();
        let override_path = temp.path().join("compose.override.yaml");
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app")
                .environment("GH_TOKEN_FILE", "/run/decune/secrets/github-token")
                .mount(ComposeOverrideMount::bind(
                    "/tmp/decune/secrets/github-token",
                    "/run/decune/secrets/github-token",
                    true,
                ))
                .secret_value_forbidden("github-test-secret"),
        );

        write_compose_override(&override_path, &patch).unwrap();

        let yaml = fs::read_to_string(override_path).unwrap();
        assert!(yaml.contains("/run/decune/secrets/github-token"));
        assert!(!yaml.contains("github-test-secret"));
    }

    #[test]
    fn compose_override_yaml_uses_placeholder_for_interpolated_environment() {
        let patch = ComposeOverridePatch::new(
            ComposeOverrideServicePatch::new("app").interpolated_environment(
                "NPM_TOKEN",
                "DECUNE_CONTAINER_ENV_NPM_TOKEN",
                vec!["secret-token".to_owned()],
            ),
        );

        let yaml = patch.to_yaml().unwrap();

        assert!(yaml.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
        assert!(!yaml.contains("secret-token"));
    }

    #[test]
    fn generated_override_file_is_passed_after_user_compose_files() {
        let (_temp, workspace) = fixture_workspace("generated-override-order");
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        write_compose_file(devcontainer_dir.join("compose.yaml"), "services: {}\n");
        write_compose_file(devcontainer_dir.join("dev.yaml"), "services: {}\n");
        let project = ComposeProjectPlan::resolve(
            &workspace,
            &devcontainer_dir,
            &["compose.yaml".into(), "dev.yaml".into()],
        )
        .unwrap();

        let command = project
            .command_plan_with_generated_override()
            .command(["config", "--format", "json"]);
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
                    .join("dev.yaml")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                workspace
                    .paths()
                    .state_dir()
                    .join("compose.override.yaml")
                    .display()
                    .to_string(),
            ]
        );
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

    #[test]
    fn compose_config_service_deserializes_startup_values() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": ["/entrypoint.sh", "--flag"],
            "command": "server --port 3000"
        }))
        .unwrap();

        assert_eq!(
            service.entrypoint,
            Some(vec!["/entrypoint.sh".to_owned(), "--flag".to_owned()])
        );
        assert_eq!(service.command, Some(vec!["server --port 3000".to_owned()]));
    }

    #[test]
    fn compose_config_service_treats_null_startup_as_image_default() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": null,
            "command": null
        }))
        .unwrap();

        assert_eq!(service.entrypoint, None);
        assert_eq!(service.command, None);
    }

    #[test]
    fn compose_config_service_preserves_empty_startup_override() {
        let service: ComposeConfigService = serde_json::from_value(serde_json::json!({
            "image": "alpine:3.20",
            "entrypoint": [],
            "command": ""
        }))
        .unwrap();

        assert_eq!(service.entrypoint, Some(Vec::new()));
        assert_eq!(service.command, Some(Vec::new()));
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

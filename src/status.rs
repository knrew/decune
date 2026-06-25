use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    config::{ConfigLayer, resolved::ResolvedDevcontainerSource},
    devcontainer::json::discover as discover_devcontainer_json,
    docker::{
        client::DockerClient,
        container::ContainerInspect,
        resource::{
            config_hash_from_labels, managed_workspace_id_from_container,
            managed_workspace_id_from_labels, workspace_path_from_labels,
        },
    },
    ports::{
        PortInventory, PortInventoryEntry, PortUsageType, collect_all_ports,
        collect_workspace_ports, render_ports_table, sort_ports,
    },
    runtime::docker_cli::{DockerCli, DockerVolumeInspect},
    state::{WorkspaceState, container_ids_match, load_state_file},
    ui,
    up::{ForwardingResolution, build_read_only_up_plan_with_forwarding_resolution},
    workspace::{Workspace, decune_state_root, is_valid_workspace_id},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusOptions {
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusInventory {
    pub(crate) workspaces: Vec<WorkspaceStatus>,
    pub(crate) issues: Vec<StatusIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspaceStatus {
    pub(crate) workspace_id: String,
    pub(crate) workspace_path: Option<String>,
    pub(crate) mode: WorkspaceMode,
    pub(crate) config_file: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) last_started_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) containers: Vec<ContainerStatusSummary>,
    pub(crate) volumes: Vec<VolumeStatusSummary>,
    pub(crate) environment_status: EnvironmentStatus,
    pub(crate) config_status: ConfigStatus,
    pub(crate) health_status: HealthStatus,
    pub(crate) lifecycle_status: LifecycleStatus,
    pub(crate) lifecycle: Option<crate::state::LifecycleState>,
    pub(crate) issues: Vec<StatusIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkspaceMode {
    Image,
    Dockerfile,
    Compose,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerStatusSummary {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) run_state: RuntimeRunState,
    pub(crate) health_status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VolumeStatusSummary {
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RuntimeRunState {
    Running,
    Stopped,
    Unknown,
}

impl WorkspaceMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Dockerfile => "dockerfile",
            Self::Compose => "compose",
            Self::Unknown => "unknown",
        }
    }
}

impl EnvironmentStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Partial => "partial",
            Self::Missing => "missing",
            Self::NotCreated => "not-created",
            Self::Unknown => "unknown",
        }
    }
}

impl ConfigStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::NeedsRebuild => "needs-rebuild",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Unknown => "unknown",
        }
    }
}

impl HealthStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Starting => "starting",
            Self::None => "none",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

impl RuntimeRunState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

impl StatusIssueSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EnvironmentStatus {
    Running,
    Stopped,
    Partial,
    Missing,
    NotCreated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConfigStatus {
    Current,
    NeedsRebuild,
    Missing,
    Unreadable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HealthStatus {
    Healthy,
    Unhealthy,
    Starting,
    None,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LifecycleStatus {
    Complete,
    Incomplete,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusIssue {
    pub(crate) code: &'static str,
    pub(crate) severity: StatusIssueSeverity,
    pub(crate) message: String,
    pub(crate) action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StatusIssueSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
struct StateEvidence {
    workspace_id: String,
    state: Result<WorkspaceState, String>,
}

#[derive(Debug, Clone, Default)]
struct DockerEvidence {
    containers: Vec<ContainerEvidence>,
    volumes: Vec<VolumeEvidence>,
}

#[derive(Debug, Clone)]
struct ComposeProjectContext {
    workspace_id: String,
    workspace_path: Option<String>,
}

#[derive(Debug, Clone)]
struct ContainerEvidence {
    workspace_id: String,
    id: Option<String>,
    name: Option<String>,
    service: Option<String>,
    workspace_path: Option<String>,
    config_hash: Option<String>,
    run_state: ContainerRunState,
    health_status: HealthStatus,
}

#[derive(Debug, Clone)]
struct VolumeEvidence {
    workspace_id: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerRunState {
    Running,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Default)]
struct WorkspaceEvidence {
    state: Option<Result<WorkspaceState, String>>,
    containers: Vec<ContainerEvidence>,
    volumes: Vec<VolumeEvidence>,
}

#[derive(Debug, Clone)]
struct CurrentWorkspaceConfig {
    mode: WorkspaceMode,
    config_file: Option<String>,
    config_hash: Option<String>,
    error: Option<String>,
}

struct WorkspaceIssueInput<'a> {
    evidence: &'a WorkspaceEvidence,
    state: Option<&'a WorkspaceState>,
    state_unreadable: bool,
    docker_unavailable: bool,
    environment_status: EnvironmentStatus,
    config_status: ConfigStatus,
    health_status: HealthStatus,
    current_config: Option<&'a CurrentWorkspaceConfig>,
}

pub(crate) async fn discover_status_inventory() -> Result<StatusInventory> {
    let state_entries = load_status_states(&decune_state_root()?)?;
    let docker_evidence = match DockerClient::connect_from_env() {
        Ok(client) => collect_docker_evidence(client.cli(), &state_entries)
            .await
            .map_err(|error| format!("Failed to read decune-managed Docker resources: {error:#}")),
        Err(error) => Err(format!("Failed to connect to Docker: {error:#}")),
    };

    Ok(build_status_inventory(state_entries, docker_evidence))
}

pub(crate) async fn run_status(options: StatusOptions) -> Result<()> {
    match options.workspace {
        Some(path) => {
            let workspace = Workspace::resolve(path)?;
            let current_config = current_workspace_config(&workspace)?;
            let status = discover_workspace_status(&workspace, current_config.clone()).await?;
            let mut ports = collect_workspace_ports(&workspace, false).await?;
            for warning in &ports.warnings {
                ui::warn(warning);
            }
            sort_ports(&mut ports.ports);
            print!("{}", render_workspace_detail(&status, &ports.ports));
        }
        None => {
            let inventory = discover_status_inventory().await?;
            for issue in &inventory.issues {
                ui::warn(&issue.message);
            }
            let mut ports = collect_all_ports().await?;
            for warning in &ports.warnings {
                ui::warn(warning);
            }
            sort_ports(&mut ports.ports);
            print!("{}", render_status_summary(&inventory, &ports));
        }
    }

    Ok(())
}

fn current_workspace_config(workspace: &Workspace) -> Result<CurrentWorkspaceConfig> {
    let config_file = match discover_devcontainer_json(workspace.root(), None) {
        Ok(path) => Some(path.display().to_string()),
        Err(error) if is_missing_devcontainer_metadata_error(&error) => return Err(error),
        Err(error) => {
            return Ok(CurrentWorkspaceConfig {
                mode: WorkspaceMode::Unknown,
                config_file: None,
                config_hash: None,
                error: Some(format!("{error:#}")),
            });
        }
    };

    match build_read_only_up_plan_with_forwarding_resolution(
        workspace,
        None,
        ConfigLayer::default(),
        ForwardingResolution::IgnoreDetached,
        false,
        false,
    ) {
        Ok(plan) => Ok(CurrentWorkspaceConfig {
            mode: mode_from_source(plan.config.devcontainer.source.as_ref()),
            config_file,
            config_hash: Some(plan.resources.config_hash),
            error: None,
        }),
        Err(error) => Ok(CurrentWorkspaceConfig {
            mode: WorkspaceMode::Unknown,
            config_file,
            config_hash: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

async fn discover_workspace_status(
    workspace: &Workspace,
    current_config: CurrentWorkspaceConfig,
) -> Result<WorkspaceStatus> {
    let state = match load_state_file(workspace.paths().state_dir()) {
        Ok(Some(state)) => Some(Ok(state)),
        Ok(None) => None,
        Err(error) => Some(Err(format!("{error:#}"))),
    };
    let docker_evidence = match DockerClient::connect_from_env() {
        Ok(client) => {
            let state_ref = state.as_ref().and_then(|state| state.as_ref().ok());
            collect_workspace_docker_evidence(client.cli(), workspace.id(), state_ref)
                .await
                .map_err(|error| {
                    format!("Failed to read decune-managed Docker resources: {error:#}")
                })
        }
        Err(error) => Err(format!("Failed to connect to Docker: {error:#}")),
    };
    let docker_unavailable = docker_evidence.is_err();
    let docker_evidence = docker_evidence.unwrap_or_default();

    let evidence = WorkspaceEvidence {
        state,
        containers: docker_evidence.containers,
        volumes: docker_evidence.volumes,
    };
    let mut status = workspace_status_with_config(
        workspace.id().to_owned(),
        evidence,
        docker_unavailable,
        Some(current_config),
    );
    status.workspace_path = Some(workspace.root().display().to_string());

    Ok(status)
}

async fn collect_workspace_docker_evidence(
    cli: &DockerCli,
    workspace_id: &str,
    state: Option<&WorkspaceState>,
) -> Result<DockerEvidence> {
    let context = ComposeProjectContext {
        workspace_id: workspace_id.to_owned(),
        workspace_path: state.map(|state| state.workspace.clone()),
    };
    let mut compose_projects = BTreeMap::<String, ComposeProjectContext>::new();
    add_compose_project_context(
        &mut compose_projects,
        state.and_then(|state| state.compose_project_name.as_deref()),
        &context,
    );

    let mut containers = Vec::new();
    for container in cli.list_workspace_container_inspects(workspace_id).await? {
        add_compose_project_context(
            &mut compose_projects,
            compose_project_name_from_container(&container).as_deref(),
            &context,
        );
        if let Some(evidence) = container_evidence(container) {
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(&project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(container, &project_context)
            }),
        );
    }
    containers = dedupe_container_evidence(containers);

    let volumes = cli
        .list_volumes(workspace_id)
        .await?
        .into_iter()
        .map(|name| VolumeEvidence {
            workspace_id: workspace_id.to_owned(),
            name: Some(name),
        })
        .collect();

    Ok(DockerEvidence {
        containers,
        volumes,
    })
}

fn load_status_states(root: &Path) -> Result<Vec<StateEvidence>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read decune state root: {}", root.display()));
        }
    };
    let mut states = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!("Failed to read decune state root entry: {}", root.display())
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(workspace_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if !is_valid_workspace_id(&workspace_id) {
            continue;
        }
        match load_state_file(&path) {
            Ok(Some(state)) => states.push(StateEvidence {
                workspace_id,
                state: Ok(state),
            }),
            Ok(None) => {}
            Err(error) => states.push(StateEvidence {
                workspace_id,
                state: Err(format!("{error:#}")),
            }),
        }
    }

    Ok(states)
}

async fn collect_docker_evidence(
    cli: &DockerCli,
    states: &[StateEvidence],
) -> Result<DockerEvidence> {
    let mut compose_projects = BTreeMap::<String, ComposeProjectContext>::new();
    for state in states {
        if let Ok(state_value) = &state.state {
            let context = ComposeProjectContext {
                workspace_id: state.workspace_id.clone(),
                workspace_path: Some(state_value.workspace.clone()),
            };
            add_compose_project_context(
                &mut compose_projects,
                state_value.compose_project_name.as_deref(),
                &context,
            );
        }
    }

    let mut containers = Vec::new();
    for container in cli.list_all_managed_container_inspects().await? {
        let project_name = compose_project_name_from_container(&container);
        if let Some(evidence) = container_evidence(container) {
            let context = ComposeProjectContext {
                workspace_id: evidence.workspace_id.clone(),
                workspace_path: evidence.workspace_path.clone(),
            };
            add_compose_project_context(&mut compose_projects, project_name.as_deref(), &context);
            containers.push(evidence);
        }
    }

    for (project_name, project_context) in compose_projects {
        let project_containers = cli
            .list_compose_project_container_inspects_by_project(&project_name)
            .await?;
        containers.extend(
            project_containers.into_iter().filter_map(|container| {
                container_evidence_with_context(container, &project_context)
            }),
        );
    }
    let containers = dedupe_container_evidence(containers);
    let volumes = cli
        .list_all_managed_volume_inspects()
        .await?
        .into_iter()
        .filter_map(volume_evidence)
        .collect();

    Ok(DockerEvidence {
        containers,
        volumes,
    })
}

fn build_status_inventory(
    states: Vec<StateEvidence>,
    docker_evidence: Result<DockerEvidence, String>,
) -> StatusInventory {
    let docker_unavailable = docker_evidence.is_err();
    let docker_error = docker_evidence.as_ref().err().cloned();
    let docker_evidence = docker_evidence.unwrap_or_default();
    let mut workspaces = BTreeMap::<String, WorkspaceEvidence>::new();

    for state in states {
        workspaces
            .entry(state.workspace_id)
            .or_default()
            .state
            .get_or_insert(state.state);
    }
    for container in docker_evidence.containers {
        workspaces
            .entry(container.workspace_id.clone())
            .or_default()
            .containers
            .push(container);
    }
    for volume in docker_evidence.volumes {
        workspaces
            .entry(volume.workspace_id.clone())
            .or_default()
            .volumes
            .push(volume);
    }

    let global_issues = if docker_unavailable {
        vec![docker_unavailable_issue(docker_error.as_deref())]
    } else {
        Vec::new()
    };
    let workspaces = workspaces
        .into_iter()
        .map(|(workspace_id, evidence)| {
            workspace_status_with_config(workspace_id, evidence, docker_unavailable, None)
        })
        .collect();

    StatusInventory {
        workspaces,
        issues: global_issues,
    }
}

fn workspace_status_with_config(
    workspace_id: String,
    evidence: WorkspaceEvidence,
    docker_unavailable: bool,
    current_config: Option<CurrentWorkspaceConfig>,
) -> WorkspaceStatus {
    let state = evidence
        .state
        .as_ref()
        .and_then(|state| state.as_ref().ok());
    let state_unreadable = evidence.state.as_ref().is_some_and(Result::is_err);
    let workspace_path = state.map(|state| state.workspace.clone()).or_else(|| {
        evidence
            .containers
            .iter()
            .find_map(|container| container.workspace_path.clone())
    });
    let environment_status = environment_status(
        &evidence,
        state,
        docker_unavailable,
        current_config.as_ref(),
    );
    let config_status = config_status(
        &evidence,
        state,
        state_unreadable,
        docker_unavailable,
        current_config.as_ref(),
    );
    let health_status = health_status(&evidence, docker_unavailable);
    let lifecycle_status = lifecycle_status(state, state_unreadable);
    let mode = current_config
        .as_ref()
        .map(|config| config.mode)
        .unwrap_or(WorkspaceMode::Unknown);
    let config_file = current_config
        .as_ref()
        .and_then(|config| config.config_file.clone())
        .or_else(|| state.and_then(|state| state.config_file.clone()));
    let issues = workspace_issues(WorkspaceIssueInput {
        evidence: &evidence,
        state,
        state_unreadable,
        docker_unavailable,
        environment_status,
        config_status,
        health_status,
        current_config: current_config.as_ref(),
    });

    WorkspaceStatus {
        workspace_id,
        workspace_path,
        mode,
        config_file,
        created_at: state.map(|state| state.created_at.clone()),
        last_started_at: state.map(|state| state.last_started_at.clone()),
        last_used_at: state.and_then(|state| state.last_used_at.clone()),
        containers: evidence
            .containers
            .iter()
            .map(ContainerStatusSummary::from)
            .collect(),
        volumes: evidence
            .volumes
            .iter()
            .map(VolumeStatusSummary::from)
            .collect(),
        environment_status,
        config_status,
        health_status,
        lifecycle_status,
        lifecycle: state.map(|state| state.lifecycle),
        issues,
    }
}

fn environment_status(
    evidence: &WorkspaceEvidence,
    state: Option<&WorkspaceState>,
    docker_unavailable: bool,
    current_config: Option<&CurrentWorkspaceConfig>,
) -> EnvironmentStatus {
    if docker_unavailable {
        return EnvironmentStatus::Unknown;
    }
    if state.is_none() && !has_docker_evidence(evidence) && current_config.is_some() {
        return EnvironmentStatus::NotCreated;
    }
    if evidence.containers.is_empty() {
        return EnvironmentStatus::Missing;
    }
    if state.is_some_and(|state| !state_container_is_present(state, &evidence.containers)) {
        return EnvironmentStatus::Partial;
    }
    if evidence
        .containers
        .iter()
        .any(|container| container.run_state == ContainerRunState::Unknown)
    {
        return EnvironmentStatus::Unknown;
    }
    let running = evidence
        .containers
        .iter()
        .filter(|container| container.run_state == ContainerRunState::Running)
        .count();
    let stopped = evidence.containers.len() - running;
    match (running, stopped) {
        (0, _) => EnvironmentStatus::Stopped,
        (_, 0) => EnvironmentStatus::Running,
        _ => EnvironmentStatus::Partial,
    }
}

fn config_status(
    evidence: &WorkspaceEvidence,
    state: Option<&WorkspaceState>,
    state_unreadable: bool,
    docker_unavailable: bool,
    current_config: Option<&CurrentWorkspaceConfig>,
) -> ConfigStatus {
    if state_unreadable {
        return ConfigStatus::Unreadable;
    }
    if current_config.is_some_and(|config| config.error.is_some()) {
        return ConfigStatus::Unreadable;
    }
    if let Some(current_hash) = current_config.and_then(|config| config.config_hash.as_deref()) {
        let hashes = docker_config_hashes(evidence);
        if !hashes.is_empty() {
            return if hashes.len() == 1 && hashes.contains(current_hash) {
                ConfigStatus::Current
            } else {
                ConfigStatus::NeedsRebuild
            };
        }
        if let Some(state) = state {
            return if state.config_hash == current_hash {
                ConfigStatus::Current
            } else {
                ConfigStatus::NeedsRebuild
            };
        }
        return ConfigStatus::Current;
    }
    if state.is_none() && has_docker_evidence(evidence) {
        return ConfigStatus::Missing;
    }
    if docker_unavailable {
        return ConfigStatus::Unknown;
    }
    let Some(state) = state else {
        return ConfigStatus::Unknown;
    };
    let hashes = docker_config_hashes(evidence);
    if hashes.is_empty() {
        return ConfigStatus::Unknown;
    }
    if hashes.len() == 1 && hashes.contains(state.config_hash.as_str()) {
        ConfigStatus::Current
    } else {
        ConfigStatus::NeedsRebuild
    }
}

fn docker_config_hashes(evidence: &WorkspaceEvidence) -> BTreeSet<&str> {
    evidence
        .containers
        .iter()
        .filter_map(|container| container.config_hash.as_deref())
        .collect()
}

fn health_status(evidence: &WorkspaceEvidence, docker_unavailable: bool) -> HealthStatus {
    if docker_unavailable {
        return HealthStatus::Unknown;
    }
    if evidence.containers.is_empty() {
        return HealthStatus::Unknown;
    }
    let statuses = evidence
        .containers
        .iter()
        .map(|container| container.health_status)
        .collect::<BTreeSet<_>>();
    if statuses.contains(&HealthStatus::Unknown) {
        return HealthStatus::Unknown;
    }
    if statuses.len() == 1 {
        statuses.into_iter().next().unwrap_or(HealthStatus::Unknown)
    } else {
        HealthStatus::Mixed
    }
}

fn lifecycle_status(state: Option<&WorkspaceState>, state_unreadable: bool) -> LifecycleStatus {
    if state_unreadable {
        return LifecycleStatus::Unknown;
    }
    let Some(state) = state else {
        return LifecycleStatus::Unknown;
    };
    let lifecycle = state.lifecycle;
    if lifecycle.on_create_completed
        && lifecycle.after_on_create_completed
        && lifecycle.update_content_completed
        && lifecycle.after_update_content_completed
        && lifecycle.post_create_completed
        && lifecycle.after_post_create_completed
    {
        LifecycleStatus::Complete
    } else {
        LifecycleStatus::Incomplete
    }
}

fn workspace_issues(input: WorkspaceIssueInput<'_>) -> Vec<StatusIssue> {
    let WorkspaceIssueInput {
        evidence,
        state,
        state_unreadable,
        docker_unavailable,
        environment_status,
        config_status,
        health_status,
        current_config,
    } = input;
    let mut issues = Vec::new();
    if docker_unavailable {
        issues.push(docker_unavailable_issue(None));
    }
    if state_unreadable {
        issues.push(issue(
            "state-unreadable",
            StatusIssueSeverity::Error,
            "The workspace state file could not be read.",
            Some("Remove or repair the state file before relying on this environment."),
        ));
    }
    if current_config
        .and_then(|config| config.error.as_deref())
        .is_some()
    {
        issues.push(StatusIssue {
            code: "config-unreadable",
            severity: StatusIssueSeverity::Warning,
            message: "The current devcontainer configuration could not be read.".to_owned(),
            action: Some("Fix the configuration error, then retry.".to_owned()),
        });
    }
    if environment_status == EnvironmentStatus::NotCreated {
        issues.push(issue(
            "not-created",
            StatusIssueSeverity::Info,
            "No decune-managed environment exists for this workspace yet.",
            Some("Run decune up to create the environment."),
        ));
    }
    if !docker_unavailable && state.is_some() && !has_docker_evidence(evidence) {
        issues.push(issue(
            "state-only",
            StatusIssueSeverity::Warning,
            "State exists but no Docker resources were found.",
            Some("Run decune up to recreate the environment, or decune remove to clean state."),
        ));
    }
    if !docker_unavailable && state.is_none() && has_docker_evidence(evidence) {
        issues.push(issue(
            "docker-only",
            StatusIssueSeverity::Warning,
            "Docker resources exist without readable decune state.",
            Some("Inspect the resources or remove them with decune remove --all-workspaces."),
        ));
    }
    if config_status == ConfigStatus::NeedsRebuild {
        issues.push(issue(
            "config-mismatch",
            StatusIssueSeverity::Warning,
            "The environment does not match the recorded configuration.",
            Some("Run decune rebuild for this workspace."),
        ));
    }
    if environment_status == EnvironmentStatus::Partial {
        issues.push(issue(
            "partial-environment",
            StatusIssueSeverity::Warning,
            "The workspace has partial or conflicting runtime evidence.",
            Some("Run decune up, down, or remove to reconcile the environment."),
        ));
    }
    if health_status == HealthStatus::Unhealthy
        || (health_status == HealthStatus::Mixed
            && evidence
                .containers
                .iter()
                .any(|container| container.health_status == HealthStatus::Unhealthy))
    {
        issues.push(issue(
            "unhealthy-container",
            StatusIssueSeverity::Error,
            "One or more containers report an unhealthy status.",
            Some("Inspect Docker health logs for the affected container."),
        ));
    }

    issues
}

fn container_evidence(container: ContainerInspect) -> Option<ContainerEvidence> {
    let (workspace_id, labels) = managed_workspace_id_from_container(&container)?;
    Some(container_evidence_from_labels(
        &container,
        workspace_id,
        labels,
        None,
    ))
}

fn container_evidence_with_context(
    container: ContainerInspect,
    context: &ComposeProjectContext,
) -> Option<ContainerEvidence> {
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let workspace_id =
        managed_workspace_id_from_labels(labels).unwrap_or_else(|| context.workspace_id.clone());
    Some(container_evidence_from_labels(
        &container,
        workspace_id,
        labels,
        context.workspace_path.as_deref(),
    ))
}

fn container_evidence_from_labels(
    container: &ContainerInspect,
    workspace_id: String,
    labels: &BTreeMap<String, String>,
    fallback_workspace_path: Option<&str>,
) -> ContainerEvidence {
    let workspace_path = workspace_path_from_labels(labels);
    let config_hash = config_hash_from_labels(labels);
    let service = compose_service_from_labels(labels);
    let run_state = container_run_state(&container.state);
    let health_status = container_health_status(&container.state);
    ContainerEvidence {
        workspace_id,
        id: container.id.clone(),
        name: container.name.clone(),
        service,
        workspace_path: workspace_path.or_else(|| fallback_workspace_path.map(str::to_owned)),
        config_hash,
        run_state,
        health_status,
    }
}

fn add_compose_project_context(
    projects: &mut BTreeMap<String, ComposeProjectContext>,
    project_name: Option<&str>,
    context: &ComposeProjectContext,
) {
    let Some(project_name) = project_name
        .map(str::trim)
        .filter(|project_name| !project_name.is_empty())
    else {
        return;
    };
    projects
        .entry(project_name.to_owned())
        .or_insert_with(|| context.clone());
}

fn compose_project_name_from_container(container: &ContainerInspect) -> Option<String> {
    container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get("com.docker.compose.project"))
        .filter(|project_name| !project_name.trim().is_empty())
        .cloned()
}

fn dedupe_container_evidence(containers: Vec<ContainerEvidence>) -> Vec<ContainerEvidence> {
    let mut positions = BTreeMap::<String, usize>::new();
    let mut deduped = Vec::<ContainerEvidence>::new();

    for container in containers {
        let id = container
            .id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned);
        let Some(id) = id else {
            deduped.push(container);
            continue;
        };

        if let Some(index) = positions.get(&id).copied() {
            if deduped[index].config_hash.is_none() && container.config_hash.is_some() {
                deduped[index] = container;
            }
        } else {
            positions.insert(id, deduped.len());
            deduped.push(container);
        }
    }

    deduped
}

fn volume_evidence(volume: DockerVolumeInspect) -> Option<VolumeEvidence> {
    let labels = volume.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    Some(VolumeEvidence {
        workspace_id,
        name: volume.name,
    })
}

impl From<&ContainerEvidence> for ContainerStatusSummary {
    fn from(value: &ContainerEvidence) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            service: value.service.clone(),
            run_state: value.run_state.into(),
            health_status: value.health_status,
        }
    }
}

impl From<ContainerRunState> for RuntimeRunState {
    fn from(value: ContainerRunState) -> Self {
        match value {
            ContainerRunState::Running => RuntimeRunState::Running,
            ContainerRunState::Stopped => RuntimeRunState::Stopped,
            ContainerRunState::Unknown => RuntimeRunState::Unknown,
        }
    }
}

impl From<&VolumeEvidence> for VolumeStatusSummary {
    fn from(value: &VolumeEvidence) -> Self {
        Self {
            name: value.name.clone(),
        }
    }
}

fn container_run_state(
    state: &Option<crate::docker::container::ContainerState>,
) -> ContainerRunState {
    let Some(state) = state else {
        return ContainerRunState::Unknown;
    };
    if state.running == Some(true) {
        return ContainerRunState::Running;
    }
    if state.running == Some(false) {
        return ContainerRunState::Stopped;
    }
    match state.status.as_deref() {
        Some("running") => ContainerRunState::Running,
        Some("created" | "exited" | "dead" | "paused" | "restarting" | "removing") => {
            ContainerRunState::Stopped
        }
        _ => ContainerRunState::Unknown,
    }
}

fn container_health_status(
    state: &Option<crate::docker::container::ContainerState>,
) -> HealthStatus {
    match state
        .as_ref()
        .and_then(|state| state.health.as_ref())
        .and_then(|health| health.status.as_deref())
    {
        Some("healthy") => HealthStatus::Healthy,
        Some("unhealthy") => HealthStatus::Unhealthy,
        Some("starting") => HealthStatus::Starting,
        Some(_) => HealthStatus::Unknown,
        None => HealthStatus::None,
    }
}

fn state_container_is_present(state: &WorkspaceState, containers: &[ContainerEvidence]) -> bool {
    containers.iter().any(|container| {
        container
            .id
            .as_deref()
            .is_some_and(|id| container_ids_match(id, &state.container_id))
    })
}

fn has_docker_evidence(evidence: &WorkspaceEvidence) -> bool {
    !evidence.containers.is_empty() || !evidence.volumes.is_empty()
}

fn mode_from_source(source: Option<&ResolvedDevcontainerSource>) -> WorkspaceMode {
    match source {
        Some(ResolvedDevcontainerSource::Image(_)) => WorkspaceMode::Image,
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => WorkspaceMode::Dockerfile,
        Some(ResolvedDevcontainerSource::Compose(_)) => WorkspaceMode::Compose,
        None => WorkspaceMode::Unknown,
    }
}

fn compose_service_from_labels(labels: &BTreeMap<String, String>) -> Option<String> {
    labels
        .get("com.docker.compose.project")
        .filter(|project| !project.trim().is_empty())?;
    labels
        .get("com.docker.compose.service")
        .filter(|service| !service.trim().is_empty())
        .cloned()
}

fn is_missing_devcontainer_metadata_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("Devcontainer metadata file was not found")
        || message.contains("Multiple devcontainer metadata files found")
}

fn render_status_summary(inventory: &StatusInventory, port_inventory: &PortInventory) -> String {
    if inventory.workspaces.is_empty() {
        return "No decune-managed workspace environments found\n".to_owned();
    }

    let mut workspaces = inventory.workspaces.iter().collect::<Vec<_>>();
    sort_workspaces_for_display(&mut workspaces);

    let mut output = String::new();
    let running = workspaces
        .iter()
        .filter(|workspace| workspace.environment_status == EnvironmentStatus::Running)
        .count();
    let stopped = workspaces
        .iter()
        .filter(|workspace| workspace.environment_status == EnvironmentStatus::Stopped)
        .count();
    let with_issues = workspaces
        .iter()
        .filter(|workspace| !workspace.issues.is_empty())
        .count();
    let _ = writeln!(
        output,
        "Found {} decune-managed workspace environments ({} running, {} stopped, {} with issues)",
        workspaces.len(),
        running,
        stopped,
        with_issues
    );

    let headers = [
        "ID",
        "WORKSPACE",
        "RUNTIME",
        "CONFIG",
        "HEALTH",
        "FWD/PUB",
        "ISSUES",
        "LAST_USED",
    ];
    let rows = workspaces
        .iter()
        .map(|workspace| summary_row(workspace, port_inventory))
        .collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in &rows {
        for (index, column) in row.iter().enumerate() {
            widths[index] = widths[index].max(column.len());
        }
    }
    write_columns(&mut output, &headers, &widths);
    for row in rows {
        let refs = row.iter().map(String::as_str).collect::<Vec<_>>();
        write_columns(&mut output, &refs, &widths);
    }

    output
}

fn render_workspace_detail(status: &WorkspaceStatus, ports: &[PortInventoryEntry]) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Workspace: {}",
        status.workspace_path.as_deref().unwrap_or("<unknown>")
    );
    let _ = writeln!(output, "ID: {}", status.workspace_id);
    let _ = writeln!(output, "Mode: {}", status.mode.as_str());
    output.push('\n');

    output.push_str("Summary\n");
    let _ = writeln!(output, "  Runtime: {}", status.environment_status.as_str());
    let _ = writeln!(output, "  Config: {}", status.config_status.as_str());
    let _ = writeln!(output, "  Health: {}", status.health_status.as_str());
    let _ = writeln!(output, "  Containers: {}", status.containers.len());
    let _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    let _ = writeln!(
        output,
        "  Last used: {}",
        format_timestamp(status.last_used_at.as_deref())
    );
    output.push('\n');

    output.push_str("Config\n");
    let _ = writeln!(
        output,
        "  File: {}",
        status.config_file.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "  Created: {}",
        status.created_at.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "  Last started: {}",
        status.last_started_at.as_deref().unwrap_or("-")
    );
    output.push('\n');

    if !status.issues.is_empty() {
        output.push_str("Issues\n");
        for issue in &status.issues {
            let _ = writeln!(
                output,
                "  {} [{}]: {}",
                issue.code,
                issue.severity.as_str(),
                issue.message
            );
        }
        output.push('\n');
    }

    if status.mode == WorkspaceMode::Compose {
        output.push_str("Services\n");
        let services = compose_services(status);
        if services.is_empty() {
            output.push_str("  -\n");
        } else {
            for service in services {
                let _ = writeln!(output, "  {service}");
            }
        }
        output.push('\n');
    }

    output.push_str("Runtime\n");
    if status.containers.is_empty() {
        output.push_str("  No containers\n");
    } else {
        for container in &status.containers {
            let name = container
                .name
                .as_deref()
                .or(container.id.as_deref())
                .unwrap_or("<unknown>");
            let service = container.service.as_deref().unwrap_or("-");
            let _ = writeln!(
                output,
                "  {}  service={}  state={}  health={}",
                name.trim_start_matches('/'),
                service,
                container.run_state.as_str(),
                container.health_status.as_str()
            );
        }
    }
    output.push('\n');

    output.push_str("Ports\n");
    for line in render_ports_table(ports, false).lines() {
        let _ = writeln!(output, "  {line}");
    }
    output.push('\n');

    output.push_str("Resources\n");
    let _ = writeln!(output, "  Containers: {}", status.containers.len());
    let _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    output.push('\n');

    if status.lifecycle_status == LifecycleStatus::Incomplete {
        output.push_str("Lifecycle\n");
        if let Some(lifecycle) = status.lifecycle {
            let _ = writeln!(
                output,
                "  onCreateCommand: {}",
                completion(
                    lifecycle.on_create_completed,
                    lifecycle.after_on_create_completed
                )
            );
            let _ = writeln!(
                output,
                "  updateContentCommand: {}",
                completion(
                    lifecycle.update_content_completed,
                    lifecycle.after_update_content_completed
                )
            );
            let _ = writeln!(
                output,
                "  postCreateCommand: {}",
                completion(
                    lifecycle.post_create_completed,
                    lifecycle.after_post_create_completed
                )
            );
        } else {
            output.push_str("  unknown\n");
        }
        output.push('\n');
    }

    let actions = status
        .issues
        .iter()
        .filter_map(|issue| issue.action.as_deref().map(|action| (issue.code, action)))
        .collect::<Vec<_>>();
    if !actions.is_empty() {
        output.push_str("Action\n");
        for (code, action) in actions {
            let _ = writeln!(output, "  {code}: {action}");
        }
    }

    output
}

fn sort_workspaces_for_display(workspaces: &mut [&WorkspaceStatus]) {
    workspaces.sort_by(|left, right| {
        (
            left.workspace_path.as_deref().unwrap_or("\u{10ffff}"),
            left.workspace_id.as_str(),
        )
            .cmp(&(
                right.workspace_path.as_deref().unwrap_or("\u{10ffff}"),
                right.workspace_id.as_str(),
            ))
    });
}

fn summary_row(workspace: &WorkspaceStatus, port_inventory: &PortInventory) -> Vec<String> {
    let (forwarded, published) = port_counts(&workspace.workspace_id, &port_inventory.ports);
    vec![
        workspace.workspace_id.clone(),
        workspace
            .workspace_path
            .as_deref()
            .unwrap_or("<unknown>")
            .to_owned(),
        workspace.environment_status.as_str().to_owned(),
        workspace.config_status.as_str().to_owned(),
        workspace.health_status.as_str().to_owned(),
        format!("{forwarded}/{published}"),
        workspace.issues.len().to_string(),
        format_timestamp(workspace.last_used_at.as_deref()),
    ]
}

fn port_counts(workspace_id: &str, ports: &[PortInventoryEntry]) -> (usize, usize) {
    let mut forwarded = 0;
    let mut published = 0;
    for port in ports
        .iter()
        .filter(|port| port.workspace_id.as_deref() == Some(workspace_id))
    {
        match port.kind {
            PortUsageType::Forwarded => forwarded += 1,
            PortUsageType::Published => published += 1,
        }
    }
    (forwarded, published)
}

fn write_columns(output: &mut String, columns: &[&str], widths: &[usize]) {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        let _ = write!(output, "{:<width$}", column, width = widths[index]);
    }
    output.push('\n');
}

fn format_timestamp(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "-".to_owned();
    };
    let Some(seconds) = value
        .strip_prefix("unix:")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return "-".to_owned();
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "-".to_owned();
    };
    let now = now.as_secs();
    if seconds > now {
        return "-".to_owned();
    }
    let elapsed = now - seconds;
    match elapsed {
        0..=59 => format!("{elapsed}s ago"),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        _ => format!("{}d ago", elapsed / 86_400),
    }
}

fn compose_services(status: &WorkspaceStatus) -> Vec<String> {
    let mut services = status
        .containers
        .iter()
        .filter_map(|container| container.service.clone())
        .collect::<Vec<_>>();
    services.sort();
    services.dedup();
    services
}

fn completion(command: bool, after_hook: bool) -> &'static str {
    match (command, after_hook) {
        (true, true) => "complete",
        (true, false) => "after-hook-pending",
        (false, _) => "pending",
    }
}

fn docker_unavailable_issue(detail: Option<&str>) -> StatusIssue {
    let message = if detail.is_some() {
        "Docker evidence could not be read.".to_owned()
    } else {
        "Docker evidence is unavailable.".to_owned()
    };
    issue(
        "docker-unavailable",
        StatusIssueSeverity::Warning,
        &message,
        Some("Check Docker availability and permissions, then retry."),
    )
}

fn issue(
    code: &'static str,
    severity: StatusIssueSeverity,
    message: &str,
    action: Option<&str>,
) -> StatusIssue {
    StatusIssue {
        code,
        severity,
        message: message.to_owned(),
        action: action.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use crate::{
        runtime::{
            command::{FakeRuntimeCommand, RuntimeOutput},
            docker_cli::DockerCli,
        },
        state::{LifecycleState, WorkspaceState},
    };

    use super::*;

    const WORKSPACE_ID: &str = "123456abcdef";
    #[test]
    fn state_only_workspace_is_reported_without_duplicates() {
        let inventory = build_status_inventory(
            vec![state_evidence(WORKSPACE_ID, state("container-id", "hash"))],
            Ok(DockerEvidence::default()),
        );

        assert_eq!(inventory.workspaces.len(), 1);
        let workspace = &inventory.workspaces[0];
        assert_eq!(workspace.workspace_id, WORKSPACE_ID);
        assert_eq!(workspace.environment_status, EnvironmentStatus::Missing);
        assert_eq!(workspace.config_status, ConfigStatus::Unknown);
        assert_issue(workspace, "state-only");
    }

    #[test]
    fn docker_only_workspace_is_reported_once() {
        let inventory = build_status_inventory(
            Vec::new(),
            Ok(DockerEvidence {
                containers: vec![container(
                    WORKSPACE_ID,
                    "container-id",
                    Some("/workspace"),
                    Some("hash"),
                    ContainerRunState::Running,
                    HealthStatus::None,
                )],
                volumes: vec![volume(WORKSPACE_ID)],
            }),
        );

        assert_eq!(inventory.workspaces.len(), 1);
        let workspace = &inventory.workspaces[0];
        assert_eq!(workspace.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(workspace.environment_status, EnvironmentStatus::Running);
        assert_eq!(workspace.config_status, ConfigStatus::Missing);
        assert_issue(workspace, "docker-only");
    }

    #[test]
    fn state_and_docker_evidence_are_merged_by_workspace_id() {
        let inventory = build_status_inventory(
            vec![state_evidence(WORKSPACE_ID, state("container-id", "hash"))],
            Ok(DockerEvidence {
                containers: vec![container(
                    WORKSPACE_ID,
                    "container-id",
                    Some("/label-path"),
                    Some("hash"),
                    ContainerRunState::Running,
                    HealthStatus::Healthy,
                )],
                volumes: vec![volume(WORKSPACE_ID)],
            }),
        );

        assert_eq!(inventory.workspaces.len(), 1);
        let workspace = &inventory.workspaces[0];
        assert_eq!(workspace.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(workspace.environment_status, EnvironmentStatus::Running);
        assert_eq!(workspace.config_status, ConfigStatus::Current);
        assert_eq!(workspace.health_status, HealthStatus::Healthy);
    }

    #[test]
    fn invalid_state_directory_ids_are_ignored() {
        let root = temp_root("invalid-state");
        fs::create_dir_all(root.join("../invalid")).unwrap();
        let invalid = root.join("not-a-valid-id");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("state.toml"), "not toml").unwrap();

        let states = load_status_states(&root).unwrap();

        assert!(states.is_empty());
    }

    #[test]
    fn invalid_docker_label_ids_are_ignored() {
        let raw_containers: Vec<ContainerInspect> = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "../victim",
                        "decune.workspace": "/workspace"
                    }
                },
                "State": { "Running": true }
            }]"#,
        )
        .unwrap();
        let raw_volumes: Vec<DockerVolumeInspect> = serde_json::from_slice(
            br#"[{
                "Name": "volume-name",
                "Labels": {
                    "decune.managed": "true",
                    "decune.workspace_id": "bad/id"
                }
            }]"#,
        )
        .unwrap();
        let inventory = build_status_inventory(
            Vec::new(),
            Ok(DockerEvidence {
                containers: raw_containers
                    .into_iter()
                    .filter_map(container_evidence)
                    .collect(),
                volumes: raw_volumes
                    .into_iter()
                    .filter_map(volume_evidence)
                    .collect(),
            }),
        );

        assert!(inventory.workspaces.is_empty());
    }

    #[test]
    fn corrupt_state_file_reports_unreadable_config_and_issue() {
        let root = temp_root("corrupt-state");
        let state_dir = root.join(WORKSPACE_ID);
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("state.toml"), "version = 'bad'").unwrap();

        let states = load_status_states(&root).unwrap();
        let inventory = build_status_inventory(states, Ok(DockerEvidence::default()));

        let workspace = single_workspace(&inventory);
        assert_eq!(workspace.config_status, ConfigStatus::Unreadable);
        assert_eq!(workspace.lifecycle_status, LifecycleStatus::Unknown);
        assert_issue(workspace, "state-unreadable");
    }

    #[test]
    fn docker_unavailable_keeps_state_workspace_and_global_issue() {
        let inventory = build_status_inventory(
            vec![state_evidence(WORKSPACE_ID, state("container-id", "hash"))],
            Err("docker failed".to_owned()),
        );

        assert_eq!(inventory.issues.len(), 1);
        assert_eq!(inventory.issues[0].code, "docker-unavailable");
        let workspace = single_workspace(&inventory);
        assert_eq!(workspace.environment_status, EnvironmentStatus::Unknown);
        assert_eq!(workspace.config_status, ConfigStatus::Unknown);
        assert_eq!(workspace.health_status, HealthStatus::Unknown);
        assert_issue(workspace, "docker-unavailable");
    }

    #[test]
    fn environment_status_derives_running_stopped_partial_missing_and_unknown() {
        assert_eq!(
            env_for(vec![ContainerRunState::Running], Some("container-0")),
            EnvironmentStatus::Running
        );
        assert_eq!(
            env_for(vec![ContainerRunState::Stopped], Some("container-0")),
            EnvironmentStatus::Stopped
        );
        assert_eq!(
            env_for(
                vec![ContainerRunState::Running, ContainerRunState::Stopped],
                Some("container-0")
            ),
            EnvironmentStatus::Partial
        );
        assert_eq!(
            build_status_inventory(
                vec![state_evidence(WORKSPACE_ID, state("container-id", "hash"))],
                Ok(DockerEvidence::default())
            )
            .workspaces[0]
                .environment_status,
            EnvironmentStatus::Missing
        );
        assert_eq!(
            env_for(vec![ContainerRunState::Unknown], Some("container-0")),
            EnvironmentStatus::Unknown
        );
        assert_eq!(
            env_for(vec![ContainerRunState::Running], Some("missing-container")),
            EnvironmentStatus::Partial
        );
    }

    #[test]
    fn health_status_derives_all_expected_values() {
        assert_eq!(
            health_for(vec![HealthStatus::Healthy]),
            HealthStatus::Healthy
        );
        assert_eq!(
            health_for(vec![HealthStatus::Unhealthy]),
            HealthStatus::Unhealthy
        );
        assert_eq!(
            health_for(vec![HealthStatus::Starting]),
            HealthStatus::Starting
        );
        assert_eq!(health_for(vec![HealthStatus::None]), HealthStatus::None);
        assert_eq!(
            health_for(vec![HealthStatus::Healthy, HealthStatus::None]),
            HealthStatus::Mixed
        );
        assert_eq!(
            build_status_inventory(
                vec![state_evidence(WORKSPACE_ID, state("container-id", "hash"))],
                Err("docker failed".to_owned())
            )
            .workspaces[0]
                .health_status,
            HealthStatus::Unknown
        );
    }

    #[test]
    fn config_mismatch_and_lifecycle_are_derived() {
        let inventory = build_status_inventory(
            vec![state_evidence(
                WORKSPACE_ID,
                WorkspaceState {
                    lifecycle: LifecycleState::all_completed(),
                    ..state("container-id", "hash")
                },
            )],
            Ok(DockerEvidence {
                containers: vec![container(
                    WORKSPACE_ID,
                    "container-id",
                    None,
                    Some("other-hash"),
                    ContainerRunState::Running,
                    HealthStatus::None,
                )],
                volumes: Vec::new(),
            }),
        );

        let workspace = single_workspace(&inventory);
        assert_eq!(workspace.config_status, ConfigStatus::NeedsRebuild);
        assert_eq!(workspace.lifecycle_status, LifecycleStatus::Complete);
        assert_issue(workspace, "config-mismatch");
    }

    #[test]
    fn current_config_uses_docker_label_hash_before_state_hash() {
        let workspace = workspace_status_with_config(
            WORKSPACE_ID.to_owned(),
            WorkspaceEvidence {
                state: Some(Ok(state("container-id", "hash"))),
                containers: vec![container(
                    WORKSPACE_ID,
                    "container-id",
                    None,
                    Some("old-hash"),
                    ContainerRunState::Running,
                    HealthStatus::None,
                )],
                volumes: Vec::new(),
            },
            false,
            Some(current_config("hash")),
        );

        assert_eq!(workspace.config_status, ConfigStatus::NeedsRebuild);
        assert_issue(&workspace, "config-mismatch");
    }

    #[test]
    fn current_config_ignores_containers_without_config_hash_labels() {
        let workspace = workspace_status_with_config(
            WORKSPACE_ID.to_owned(),
            WorkspaceEvidence {
                state: Some(Ok(state("container-id", "old-hash"))),
                containers: vec![
                    container(
                        WORKSPACE_ID,
                        "container-id",
                        None,
                        Some("hash"),
                        ContainerRunState::Running,
                        HealthStatus::None,
                    ),
                    container(
                        WORKSPACE_ID,
                        "sidecar-id",
                        None,
                        None,
                        ContainerRunState::Running,
                        HealthStatus::None,
                    ),
                ],
                volumes: Vec::new(),
            },
            false,
            Some(current_config("hash")),
        );

        assert_eq!(workspace.config_status, ConfigStatus::Current);
        assert!(
            !workspace
                .issues
                .iter()
                .any(|issue| issue.code == "config-mismatch"),
            "{:?}",
            workspace.issues
        );
    }

    #[test]
    fn public_model_debug_does_not_include_raw_sensitive_evidence() {
        let inventory = build_status_inventory(
            vec![state_evidence(
                WORKSPACE_ID,
                state("container-id", "secret-config-hash"),
            )],
            Ok(DockerEvidence {
                containers: vec![container(
                    WORKSPACE_ID,
                    "container-id",
                    Some("/workspace"),
                    Some("secret-config-hash"),
                    ContainerRunState::Running,
                    HealthStatus::None,
                )],
                volumes: Vec::new(),
            }),
        );

        let debug = format!("{inventory:?}");
        assert!(!debug.contains("secret-config-hash"));
        assert!(!debug.contains("decune.config_hash"));
        assert!(!debug.contains("TOKEN="));
        assert!(!debug.contains("Labels"));
    }

    #[test]
    fn container_and_volume_inspect_are_reduced_to_valid_evidence() {
        let container: Vec<ContainerInspect> = serde_json::from_slice(
            br#"[{
                "Id": "container-id",
                "Config": {
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef",
                        "decune.workspace": "/workspace",
                        "decune.config_hash": "hash"
                    }
                },
                "State": {
                    "Status": "running",
                    "Health": { "Status": "healthy" }
                }
            }]"#,
        )
        .unwrap();
        let evidence = container_evidence(container.into_iter().next().unwrap()).unwrap();

        assert_eq!(evidence.workspace_id, WORKSPACE_ID);
        assert_eq!(evidence.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(evidence.run_state, ContainerRunState::Running);
        assert_eq!(evidence.health_status, HealthStatus::Healthy);
    }

    #[test]
    fn workspace_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": {
                        "Running": true,
                        "Health": { "Status": "healthy" }
                    }
                },{
                    "Id": "sidecar-id",
                    "Name": "/project-db-1",
                    "Config": {
                        "Labels": {
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "db"
                        }
                    },
                    "State": {
                        "Running": false,
                        "Health": { "Status": "unhealthy" }
                    }
                }]"#,
            )),
            Ok(output(
                br#"{"ID":"primary-id"}
{"ID":"sidecar-id"}
"#,
            )),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": {
                        "Running": true,
                        "Health": { "Status": "healthy" }
                    }
                }]"#,
            )),
            Ok(output(br#"{"ID":"primary-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };

        let evidence = runtime
            .block_on(collect_workspace_docker_evidence(
                &cli,
                WORKSPACE_ID,
                Some(&state),
            ))
            .unwrap();

        assert_eq!(evidence.containers.len(), 2);
        let sidecar = evidence
            .containers
            .iter()
            .find(|container| container.id.as_deref() == Some("sidecar-id"))
            .unwrap();
        assert_eq!(sidecar.workspace_id, WORKSPACE_ID);
        assert_eq!(sidecar.workspace_path.as_deref(), Some("/workspace"));
        assert_eq!(sidecar.service.as_deref(), Some("db"));
        assert_eq!(sidecar.run_state, ContainerRunState::Stopped);
        assert_eq!(sidecar.health_status, HealthStatus::Unhealthy);

        let status = workspace_status_with_config(
            WORKSPACE_ID.to_owned(),
            WorkspaceEvidence {
                state: Some(Ok(state)),
                containers: evidence.containers,
                volumes: evidence.volumes,
            },
            false,
            Some(CurrentWorkspaceConfig {
                mode: WorkspaceMode::Compose,
                config_file: Some("/workspace/.devcontainer/devcontainer.json".to_owned()),
                config_hash: Some("hash".to_owned()),
                error: None,
            }),
        );
        assert_eq!(status.environment_status, EnvironmentStatus::Partial);
        assert_eq!(status.health_status, HealthStatus::Mixed);
        assert_issue(&status, "partial-environment");
        assert_issue(&status, "unhealthy-container");

        let services = compose_services(&status);
        assert_eq!(services, vec!["app".to_owned(), "db".to_owned()]);
        let commands = runner.commands();
        assert!(commands.iter().any(|command| {
            command
                .args_vec()
                .contains(&"label=com.docker.compose.project=project".to_owned())
        }));
    }

    #[test]
    fn all_docker_evidence_includes_compose_sidecar_from_state_project() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(b"")),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": { "Running": true }
                },{
                    "Id": "sidecar-id",
                    "Name": "/project-db-1",
                    "Config": {
                        "Labels": {
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "db"
                        }
                    },
                    "State": { "Running": false }
                }]"#,
            )),
            Ok(output(
                br#"{"ID":"primary-id"}
{"ID":"sidecar-id"}
"#,
            )),
            Ok(output(
                br#"[{
                    "Id": "primary-id",
                    "Name": "/project-app-1",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef",
                            "decune.workspace": "/workspace",
                            "decune.config_hash": "hash",
                            "com.docker.compose.project": "project",
                            "com.docker.compose.service": "app"
                        }
                    },
                    "State": { "Running": true }
                }]"#,
            )),
            Ok(output(br#"{"ID":"primary-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = WorkspaceState {
            compose_project_name: Some("project".to_owned()),
            ..state("primary-id", "hash")
        };
        let states = vec![state_evidence(WORKSPACE_ID, state)];

        let evidence = runtime
            .block_on(collect_docker_evidence(&cli, &states))
            .unwrap();

        assert_eq!(evidence.containers.len(), 2);
        assert!(evidence.containers.iter().any(|container| {
            container.id.as_deref() == Some("sidecar-id")
                && container.service.as_deref() == Some("db")
                && container.workspace_id == WORKSPACE_ID
        }));
    }

    #[test]
    fn docker_evidence_collection_uses_read_only_commands() {
        let runner = FakeRuntimeCommand::new(vec![
            Ok(output(
                br#"[{
                    "Name": "volume-name",
                    "Labels": {
                        "decune.managed": "true",
                        "decune.workspace_id": "123456abcdef"
                    }
                }]"#,
            )),
            Ok(output(b"volume-name\n")),
            Ok(output(
                br#"[{
                    "Id": "container-id",
                    "Config": {
                        "Labels": {
                            "decune.managed": "true",
                            "decune.workspace_id": "123456abcdef"
                        }
                    },
                    "State": { "Running": true }
                }]"#,
            )),
            Ok(output(br#"{"ID":"container-id"}"#)),
        ]);
        let cli = DockerCli::new(Arc::new(runner.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let evidence = runtime
            .block_on(collect_docker_evidence(&cli, &[]))
            .unwrap();

        assert_eq!(evidence.containers.len(), 1);
        assert_eq!(evidence.volumes.len(), 1);
        let commands = runner.commands();
        let args = commands
            .iter()
            .map(|command| command.args_vec().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                vec![
                    "ps",
                    "--all",
                    "--filter",
                    "label=decune.managed=true",
                    "--format",
                    "json",
                ],
                vec!["container", "inspect", "container-id"],
                vec![
                    "volume",
                    "ls",
                    "--filter",
                    "label=decune.managed=true",
                    "--format",
                    "{{.Name}}",
                ],
                vec!["volume", "inspect", "volume-name"],
            ]
        );
        for command in commands {
            let args = command.args_vec();
            assert!(
                matches!(
                    args,
                    [first, second, ..] if first == "container" && second == "inspect"
                ) || matches!(
                    args,
                    [first, second, ..] if first == "volume" && (second == "ls" || second == "inspect")
                ) || matches!(args, [first, ..] if first == "ps"),
                "{args:?}"
            );
        }
    }

    #[test]
    fn summary_renderer_reports_empty_inventory() {
        let output = render_status_summary(
            &StatusInventory {
                workspaces: Vec::new(),
                issues: Vec::new(),
            },
            &PortInventory::default(),
        );

        assert_eq!(output, "No decune-managed workspace environments found\n");
    }

    #[test]
    fn summary_renderer_sorts_paths_and_does_not_fallback_last_used() {
        let mut alpha = rendered_status("bbbbbbbbbbbb", Some("/alpha"));
        alpha.created_at = Some("unix:1".to_owned());
        alpha.last_started_at = Some("unix:2".to_owned());
        let beta = rendered_status("aaaaaaaaaaaa", Some("/beta"));
        let unknown = rendered_status("cccccccccccc", None);
        let output = render_status_summary(
            &StatusInventory {
                workspaces: vec![unknown, beta, alpha],
                issues: Vec::new(),
            },
            &PortInventory::default(),
        );

        let alpha_index = output.find("/alpha").unwrap();
        let beta_index = output.find("/beta").unwrap();
        let unknown_index = output.find("<unknown>").unwrap();
        assert!(alpha_index < beta_index);
        assert!(beta_index < unknown_index);
        assert!(output.contains("LAST_USED"));
        assert!(output.lines().any(|line| {
            line.contains("bbbbbbbbbbbb") && line.split_whitespace().last() == Some("-")
        }));
    }

    #[test]
    fn detail_renderer_reports_not_created_and_omits_complete_lifecycle() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.mode = WorkspaceMode::Image;
        status.environment_status = EnvironmentStatus::NotCreated;
        status.lifecycle_status = LifecycleStatus::Complete;
        status.lifecycle = Some(LifecycleState::all_completed());
        status.issues.push(issue(
            "not-created",
            StatusIssueSeverity::Info,
            "No decune-managed environment exists for this workspace yet.",
            Some("Run decune up to create the environment."),
        ));

        let output = render_workspace_detail(&status, &[]);

        assert!(output.contains("Runtime: not-created"));
        assert!(output.contains("No active ports for this workspace"));
        assert!(output.contains("Run decune up to create the environment."));
        assert!(!output.contains("Lifecycle\n"));
    }

    #[test]
    fn detail_renderer_reports_issue_codes_severities_and_all_actions() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.issues.push(issue(
            "config-unreadable",
            StatusIssueSeverity::Warning,
            "The current devcontainer configuration could not be read.",
            Some("Fix the configuration error, then retry."),
        ));
        status.issues.push(issue(
            "not-created",
            StatusIssueSeverity::Info,
            "No decune-managed environment exists for this workspace yet.",
            Some("Run decune up to create the environment."),
        ));

        let output = render_workspace_detail(&status, &[]);

        assert!(output.contains(
            "config-unreadable [warning]: The current devcontainer configuration could not be read."
        ));
        assert!(output.contains(
            "not-created [info]: No decune-managed environment exists for this workspace yet."
        ));
        assert!(output.contains("config-unreadable: Fix the configuration error, then retry."));
        assert!(output.contains("not-created: Run decune up to create the environment."));
    }

    #[test]
    fn renderers_do_not_include_sensitive_raw_values() {
        let mut status = rendered_status(WORKSPACE_ID, Some("/workspace"));
        status.config_file = Some("/workspace/.devcontainer/devcontainer.json".to_owned());
        status.issues.push(StatusIssue {
            code: "config-unreadable",
            severity: StatusIssueSeverity::Warning,
            message: "The current devcontainer configuration could not be read.".to_owned(),
            action: Some("Fix the configuration error, then retry.".to_owned()),
        });
        let output = render_workspace_detail(&status, &[]);

        assert!(!output.contains("secret-config-hash"));
        assert!(!output.contains("decune.config_hash"));
        assert!(!output.contains("TOKEN="));
        assert!(!output.contains("build.args"));
        assert!(!output.contains("raw-compose"));
    }

    fn env_for(
        states: Vec<ContainerRunState>,
        state_container_id: Option<&str>,
    ) -> EnvironmentStatus {
        let containers = states
            .into_iter()
            .enumerate()
            .map(|(index, run_state)| {
                container(
                    WORKSPACE_ID,
                    &format!("container-{index}"),
                    None,
                    Some("hash"),
                    run_state,
                    HealthStatus::None,
                )
            })
            .collect::<Vec<_>>();
        let state = state_container_id.map(|id| state(id, "hash"));
        let states = state
            .map(|state| vec![state_evidence(WORKSPACE_ID, state)])
            .unwrap_or_default();
        build_status_inventory(
            states,
            Ok(DockerEvidence {
                containers,
                volumes: Vec::new(),
            }),
        )
        .workspaces[0]
            .environment_status
    }

    fn health_for(statuses: Vec<HealthStatus>) -> HealthStatus {
        let containers = statuses
            .into_iter()
            .enumerate()
            .map(|(index, health)| {
                container(
                    WORKSPACE_ID,
                    &format!("container-{index}"),
                    None,
                    Some("hash"),
                    ContainerRunState::Running,
                    health,
                )
            })
            .collect();
        build_status_inventory(
            Vec::new(),
            Ok(DockerEvidence {
                containers,
                volumes: Vec::new(),
            }),
        )
        .workspaces[0]
            .health_status
    }

    fn single_workspace(inventory: &StatusInventory) -> &WorkspaceStatus {
        assert_eq!(inventory.workspaces.len(), 1);
        &inventory.workspaces[0]
    }

    fn assert_issue(workspace: &WorkspaceStatus, code: &str) {
        assert!(
            workspace.issues.iter().any(|issue| issue.code == code),
            "{:?}",
            workspace.issues
        );
    }

    fn state_evidence(workspace_id: &str, state: WorkspaceState) -> StateEvidence {
        StateEvidence {
            workspace_id: workspace_id.to_owned(),
            state: Ok(state),
        }
    }

    fn state(container_id: &str, config_hash: &str) -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspace: "/workspace".to_owned(),
            container_id: container_id.to_owned(),
            image: "image".to_owned(),
            config_hash: config_hash.to_owned(),
            config_file: None,
            compose_project_name: None,
            published_ports: Vec::new(),
            created_at: "unix:1".to_owned(),
            last_started_at: "unix:2".to_owned(),
            last_used_at: None,
            lifecycle: LifecycleState::default(),
        }
    }

    fn current_config(config_hash: &str) -> CurrentWorkspaceConfig {
        CurrentWorkspaceConfig {
            mode: WorkspaceMode::Image,
            config_file: Some("/workspace/.devcontainer/devcontainer.json".to_owned()),
            config_hash: Some(config_hash.to_owned()),
            error: None,
        }
    }

    fn container(
        workspace_id: &str,
        id: &str,
        workspace_path: Option<&str>,
        config_hash: Option<&str>,
        run_state: ContainerRunState,
        health_status: HealthStatus,
    ) -> ContainerEvidence {
        ContainerEvidence {
            workspace_id: workspace_id.to_owned(),
            id: Some(id.to_owned()),
            name: Some(format!("/{id}")),
            service: None,
            workspace_path: workspace_path.map(str::to_owned),
            config_hash: config_hash.map(str::to_owned),
            run_state,
            health_status,
        }
    }

    fn volume(workspace_id: &str) -> VolumeEvidence {
        VolumeEvidence {
            workspace_id: workspace_id.to_owned(),
            name: None,
        }
    }

    fn rendered_status(workspace_id: &str, workspace_path: Option<&str>) -> WorkspaceStatus {
        WorkspaceStatus {
            workspace_id: workspace_id.to_owned(),
            workspace_path: workspace_path.map(str::to_owned),
            mode: WorkspaceMode::Unknown,
            config_file: None,
            created_at: None,
            last_started_at: None,
            last_used_at: None,
            containers: Vec::new(),
            volumes: Vec::new(),
            environment_status: EnvironmentStatus::Missing,
            config_status: ConfigStatus::Unknown,
            health_status: HealthStatus::Unknown,
            lifecycle_status: LifecycleStatus::Unknown,
            lifecycle: None,
            issues: Vec::new(),
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("decune-status-tests")
            .join(std::process::id().to_string())
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn output(stdout: &[u8]) -> RuntimeOutput {
        RuntimeOutput {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        }
    }
}

#![expect(
    dead_code,
    reason = "Status inventory is implemented before the CLI status surface consumes it."
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    docker::{
        client::DockerClient,
        container::ContainerInspect,
        resource::{
            config_hash_from_labels, managed_workspace_id_from_container,
            managed_workspace_id_from_labels, workspace_path_from_labels,
        },
    },
    runtime::docker_cli::{DockerCli, DockerVolumeInspect},
    state::{WorkspaceState, container_ids_match, load_state_file},
    workspace::{decune_state_root, is_valid_workspace_id},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusInventory {
    pub(crate) workspaces: Vec<WorkspaceStatus>,
    pub(crate) issues: Vec<StatusIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspaceStatus {
    pub(crate) workspace_id: String,
    pub(crate) workspace_path: Option<String>,
    pub(crate) environment_status: EnvironmentStatus,
    pub(crate) config_status: ConfigStatus,
    pub(crate) health_status: HealthStatus,
    pub(crate) lifecycle_status: LifecycleStatus,
    pub(crate) issues: Vec<StatusIssue>,
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
struct ContainerEvidence {
    workspace_id: String,
    id: Option<String>,
    workspace_path: Option<String>,
    config_hash: Option<String>,
    run_state: ContainerRunState,
    health_status: HealthStatus,
}

#[derive(Debug, Clone)]
struct VolumeEvidence {
    workspace_id: String,
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

pub(crate) async fn discover_status_inventory() -> Result<StatusInventory> {
    let state_entries = load_status_states(&decune_state_root()?)?;
    let docker_evidence = match DockerClient::connect_from_env() {
        Ok(client) => collect_docker_evidence(client.cli())
            .await
            .map_err(|error| format!("Failed to read decune-managed Docker resources: {error:#}")),
        Err(error) => Err(format!("Failed to connect to Docker: {error:#}")),
    };

    Ok(build_status_inventory(state_entries, docker_evidence))
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

async fn collect_docker_evidence(cli: &DockerCli) -> Result<DockerEvidence> {
    let containers = cli
        .list_all_managed_container_inspects()
        .await?
        .into_iter()
        .filter_map(container_evidence)
        .collect();
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
            workspace_status(workspace_id, evidence, docker_unavailable)
        })
        .collect();

    StatusInventory {
        workspaces,
        issues: global_issues,
    }
}

fn workspace_status(
    workspace_id: String,
    evidence: WorkspaceEvidence,
    docker_unavailable: bool,
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
    let environment_status = environment_status(&evidence, state, docker_unavailable);
    let config_status = config_status(&evidence, state, state_unreadable, docker_unavailable);
    let health_status = health_status(&evidence, docker_unavailable);
    let lifecycle_status = lifecycle_status(state, state_unreadable);
    let issues = workspace_issues(
        &evidence,
        state,
        state_unreadable,
        docker_unavailable,
        environment_status,
        config_status,
        health_status,
    );

    WorkspaceStatus {
        workspace_id,
        workspace_path,
        environment_status,
        config_status,
        health_status,
        lifecycle_status,
        issues,
    }
}

fn environment_status(
    evidence: &WorkspaceEvidence,
    state: Option<&WorkspaceState>,
    docker_unavailable: bool,
) -> EnvironmentStatus {
    if docker_unavailable {
        return EnvironmentStatus::Unknown;
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
) -> ConfigStatus {
    if state_unreadable {
        return ConfigStatus::Unreadable;
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
    let hashes = evidence
        .containers
        .iter()
        .filter_map(|container| container.config_hash.as_deref())
        .collect::<BTreeSet<_>>();
    if hashes.is_empty() {
        return ConfigStatus::Unknown;
    }
    if hashes.len() == 1 && hashes.contains(state.config_hash.as_str()) {
        ConfigStatus::Current
    } else {
        ConfigStatus::NeedsRebuild
    }
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

fn workspace_issues(
    evidence: &WorkspaceEvidence,
    state: Option<&WorkspaceState>,
    state_unreadable: bool,
    docker_unavailable: bool,
    environment_status: EnvironmentStatus,
    config_status: ConfigStatus,
    health_status: HealthStatus,
) -> Vec<StatusIssue> {
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
    let workspace_path = workspace_path_from_labels(labels);
    let config_hash = config_hash_from_labels(labels);
    let run_state = container_run_state(&container.state);
    let health_status = container_health_status(&container.state);
    Some(ContainerEvidence {
        workspace_id,
        id: container.id,
        workspace_path,
        config_hash,
        run_state,
        health_status,
    })
}

fn volume_evidence(volume: DockerVolumeInspect) -> Option<VolumeEvidence> {
    let labels = volume.labels.as_ref()?;
    let workspace_id = managed_workspace_id_from_labels(labels)?;
    Some(VolumeEvidence { workspace_id })
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

        let evidence = runtime.block_on(collect_docker_evidence(&cli)).unwrap();

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
            created_at: "unix:1".to_owned(),
            last_started_at: "unix:2".to_owned(),
            last_used_at: None,
            lifecycle: LifecycleState::default(),
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
            workspace_path: workspace_path.map(str::to_owned),
            config_hash: config_hash.map(str::to_owned),
            run_state,
            health_status,
        }
    }

    fn volume(workspace_id: &str) -> VolumeEvidence {
        VolumeEvidence {
            workspace_id: workspace_id.to_owned(),
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

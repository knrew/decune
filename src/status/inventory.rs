use std::collections::{BTreeMap, BTreeSet};

use crate::state::WorkspaceState;

use super::{
    evidence::{
        ContainerRunState, CurrentWorkspaceConfig, DockerEvidence, StateEvidence,
        WorkspaceEvidence, has_docker_evidence, state_container_is_present,
    },
    types::{
        ConfigStatus, ContainerStatusSummary, EnvironmentStatus, HealthStatus, LifecycleStatus,
        StatusInventory, StatusIssue, StatusIssueSeverity, VolumeStatusSummary, WorkspaceMode,
        WorkspaceStatus,
    },
};

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

pub(super) fn build_status_inventory(
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

pub(super) fn workspace_status_with_config(
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
        .map_or(WorkspaceMode::Unknown, |config| config.mode);
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

const fn lifecycle_status(
    state: Option<&WorkspaceState>,
    state_unreadable: bool,
) -> LifecycleStatus {
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
    use std::{fs, path::PathBuf};

    use crate::{
        state::{LifecycleState, WorkspaceState},
        status::evidence::{
            ContainerEvidence, ContainerRunState, CurrentWorkspaceConfig, DockerEvidence,
            StateEvidence, VolumeEvidence, WorkspaceEvidence, load_status_states,
        },
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

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("decune-status-tests")
            .join(std::process::id().to_string())
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
}

use std::{collections::BTreeSet, fmt, fmt::Write as _};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    docker::{
        container::{ContainerInspect, ContainerState},
        resource::{
            compose_project_name_from_labels, compose_service_name_from_labels,
            config_hash_from_labels, managed_workspace_id_from_labels,
        },
    },
    ports::{
        ContainerPortSnapshot, PortInventoryEntry, container_port_inventory, render_ports_table,
    },
    state::{
        LifecycleCompletion, LifecycleState, WorkspaceModeSnapshot, WorkspaceState,
        container_ids_match,
    },
};

use super::aggregate::{
    aggregate_environment_status, aggregate_health_status, aggregate_lifecycle_status,
    should_report_unhealthy,
};
use super::types::{
    ContainerStatusSummary, EnvironmentStatus, LifecycleStatus, StatusIssueSeverity,
    VolumeStatusSummary, WorkspaceMode,
};
pub(crate) use super::types::{HealthStatus, RuntimeRunState};

const SNAPSHOT_IDENTITY_DOMAIN: &[u8] = b"decune-container-query-config-identity-v1";

#[derive(Clone, PartialEq, Eq)]
struct SnapshotIdentity([u8; 32]);

impl SnapshotIdentity {
    fn from_config_hash(config_hash: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(SNAPSHOT_IDENTITY_DOMAIN);
        hasher.update([0]);
        hasher.update(config_hash.as_bytes());
        Self(hasher.finalize().into())
    }
}

impl fmt::Debug for SnapshotIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotIdentity(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerQueryStateSnapshot {
    pub(crate) mode: WorkspaceModeSnapshot,
    pub(crate) primary_container_id: String,
    pub(crate) created_at: String,
    pub(crate) last_started_at: String,
    pub(crate) last_used_at: Option<String>,
    pub(crate) lifecycle: LifecycleState,
    #[serde(skip_serializing)]
    config_identity: SnapshotIdentity,
}

impl ContainerQueryStateSnapshot {
    pub(crate) fn from_state(state: &WorkspaceState) -> Self {
        Self {
            mode: state.mode,
            primary_container_id: state.container_id.clone(),
            created_at: state.created_at.clone(),
            last_started_at: state.last_started_at.clone(),
            last_used_at: state.last_used_at.clone(),
            lifecycle: state.lifecycle,
            config_identity: SnapshotIdentity::from_config_hash(&state.config_hash),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "availability", content = "snapshot")]
pub(crate) enum ContainerQueryStateEvidence {
    Available(ContainerQueryStateSnapshot),
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerQueryContainerEvidence {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) run_state: RuntimeRunState,
    pub(crate) health_status: HealthStatus,
    #[serde(skip_serializing)]
    config_identity: Option<SnapshotIdentity>,
}

impl ContainerQueryContainerEvidence {
    pub(crate) fn new(
        id: Option<String>,
        name: Option<String>,
        service: Option<String>,
        run_state: RuntimeRunState,
        health_status: HealthStatus,
        config_hash: Option<&str>,
    ) -> Self {
        Self {
            id,
            name,
            service,
            run_state,
            health_status,
            config_identity: config_hash.map(SnapshotIdentity::from_config_hash),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub(crate) struct ContainerQueryContainersEvidence {
    pub(crate) containers: Vec<ContainerQueryContainerEvidence>,
    pub(crate) published_ports: Vec<ContainerPortSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerQueryVolumeEvidence {
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub(crate) struct ContainerQueryRuntimeSnapshot {
    pub(crate) containers: Vec<ContainerQueryContainerEvidence>,
    pub(crate) volumes: Vec<ContainerQueryVolumeEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "availability", content = "snapshot")]
pub(crate) enum ContainerQueryDockerEvidence {
    Available(ContainerQueryRuntimeSnapshot),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerQuerySnapshot {
    pub(crate) workspace_id: String,
    pub(crate) state: ContainerQueryStateEvidence,
    pub(crate) docker: ContainerQueryDockerEvidence,
    pub(crate) ports: Vec<ContainerPortSnapshot>,
}

pub(crate) fn container_query_evidence_from_inspect(
    container: &ContainerInspect,
    workspace_id: &str,
    compose_projects: &BTreeSet<String>,
) -> Option<ContainerQueryContainerEvidence> {
    if !container_query_inspect_matches_scope(container, workspace_id, compose_projects) {
        return None;
    }
    let labels = container.config.as_ref()?.labels.as_ref()?;
    let managed_workspace_id = managed_workspace_id_from_labels(labels);
    let managed_for_workspace = managed_workspace_id.as_deref() == Some(workspace_id);

    Some(ContainerQueryContainerEvidence::new(
        container.id.clone(),
        container.name.clone(),
        compose_service_name_from_labels(labels),
        container_query_run_state(container.state.as_ref()),
        container_query_health_status(container.state.as_ref()),
        managed_for_workspace
            .then(|| config_hash_from_labels(labels))
            .flatten()
            .as_deref(),
    ))
}

pub(crate) fn container_query_inspect_matches_scope(
    container: &ContainerInspect,
    workspace_id: &str,
    compose_projects: &BTreeSet<String>,
) -> bool {
    let Some(labels) = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
    else {
        return false;
    };
    let managed_workspace_id = managed_workspace_id_from_labels(labels);
    if managed_workspace_id.is_some() {
        return managed_workspace_id.as_deref() == Some(workspace_id);
    }
    compose_project_name_from_labels(labels)
        .as_ref()
        .is_some_and(|project| compose_projects.contains(project))
}

fn container_query_run_state(state: Option<&ContainerState>) -> RuntimeRunState {
    let Some(state) = state else {
        return RuntimeRunState::Unknown;
    };
    if state.running == Some(true) {
        return RuntimeRunState::Running;
    }
    if state.running == Some(false) {
        return RuntimeRunState::Stopped;
    }
    match state.status.as_deref() {
        Some("running") => RuntimeRunState::Running,
        Some("created" | "exited" | "dead" | "paused" | "restarting" | "removing") => {
            RuntimeRunState::Stopped
        }
        Some(_) | None => RuntimeRunState::Unknown,
    }
}

fn container_query_health_status(state: Option<&ContainerState>) -> HealthStatus {
    match state
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConfigSnapshotStatus {
    Consistent,
    RuntimeMismatch,
    Unavailable,
}

impl ConfigSnapshotStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Consistent => "consistent",
            Self::RuntimeMismatch => "runtime-mismatch",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LiveWorkspaceStatus {
    NotChecked,
}

impl LiveWorkspaceStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotChecked => "not checked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerQueryStatusIssue {
    pub(crate) code: &'static str,
    pub(crate) severity: StatusIssueSeverity,
    pub(crate) message: &'static str,
    pub(crate) action: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ContainerWorkspaceStatus {
    pub(crate) workspace_id: String,
    pub(crate) mode: WorkspaceMode,
    pub(crate) config_snapshot_status: ConfigSnapshotStatus,
    pub(crate) live_workspace_status: LiveWorkspaceStatus,
    pub(crate) environment_status: EnvironmentStatus,
    pub(crate) health_status: HealthStatus,
    pub(crate) lifecycle_status: LifecycleStatus,
    pub(crate) lifecycle: Option<LifecycleState>,
    pub(crate) created_at: Option<String>,
    pub(crate) last_started_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) containers: Vec<ContainerStatusSummary>,
    pub(crate) volumes: Vec<VolumeStatusSummary>,
    pub(crate) ports: Vec<PortInventoryEntry>,
    pub(crate) issues: Vec<ContainerQueryStatusIssue>,
}

pub(crate) fn build_container_workspace_status(
    snapshot: &ContainerQuerySnapshot,
) -> ContainerWorkspaceStatus {
    let state = available_state(&snapshot.state);
    let runtime = available_runtime(&snapshot.docker);
    let environment_status = container_environment_status(state, runtime);
    let health_status = container_health_status(runtime);
    let config_snapshot_status = config_snapshot_status(state, runtime);
    let lifecycle_status = container_lifecycle_status(state);
    let issues = container_status_issues(
        config_snapshot_status,
        environment_status,
        health_status,
        lifecycle_status,
        runtime,
    );

    ContainerWorkspaceStatus {
        workspace_id: snapshot.workspace_id.clone(),
        mode: state.map_or(WorkspaceMode::Unknown, |state| state.mode.into()),
        config_snapshot_status,
        live_workspace_status: LiveWorkspaceStatus::NotChecked,
        environment_status,
        health_status,
        lifecycle_status,
        lifecycle: state.map(|state| state.lifecycle),
        created_at: state.map(|state| state.created_at.clone()),
        last_started_at: state.map(|state| state.last_started_at.clone()),
        last_used_at: state.and_then(|state| state.last_used_at.clone()),
        containers: runtime.map_or_else(Vec::new, |runtime| {
            runtime
                .containers
                .iter()
                .map(|container| ContainerStatusSummary {
                    id: container.id.clone(),
                    name: container.name.clone(),
                    service: container.service.clone(),
                    run_state: container.run_state,
                    health_status: container.health_status,
                })
                .collect()
        }),
        volumes: runtime.map_or_else(Vec::new, |runtime| {
            runtime
                .volumes
                .iter()
                .map(|volume| VolumeStatusSummary {
                    name: volume.name.clone(),
                })
                .collect()
        }),
        ports: container_port_inventory(&snapshot.ports),
        issues,
    }
}

pub(crate) fn render_container_workspace_status(status: &ContainerWorkspaceStatus) -> String {
    let mut output = String::new();
    write_container_header(&mut output, status);
    write_container_summary(&mut output, status);
    write_recorded_state(&mut output, status);
    write_container_issues(&mut output, status);
    write_container_runtime(&mut output, status);
    write_container_ports(&mut output, status);
    write_container_resources(&mut output, status);
    write_container_lifecycle(&mut output, status);
    write_host_actions(&mut output, status);
    finish_with_single_newline(output)
}

const fn available_state(
    evidence: &ContainerQueryStateEvidence,
) -> Option<&ContainerQueryStateSnapshot> {
    match evidence {
        ContainerQueryStateEvidence::Available(state) => Some(state),
        ContainerQueryStateEvidence::Missing | ContainerQueryStateEvidence::Unavailable => None,
    }
}

const fn available_runtime(
    evidence: &ContainerQueryDockerEvidence,
) -> Option<&ContainerQueryRuntimeSnapshot> {
    match evidence {
        ContainerQueryDockerEvidence::Available(runtime) => Some(runtime),
        ContainerQueryDockerEvidence::Unavailable => None,
    }
}

fn config_snapshot_status(
    state: Option<&ContainerQueryStateSnapshot>,
    runtime: Option<&ContainerQueryRuntimeSnapshot>,
) -> ConfigSnapshotStatus {
    let (Some(state), Some(runtime)) = (state, runtime) else {
        return ConfigSnapshotStatus::Unavailable;
    };
    let Some(primary) = runtime.containers.iter().find(|container| {
        container.id.as_deref().is_some_and(|container_id| {
            container_ids_match(container_id, &state.primary_container_id)
        })
    }) else {
        return ConfigSnapshotStatus::RuntimeMismatch;
    };
    if runtime
        .containers
        .iter()
        .filter_map(|container| container.config_identity.as_ref())
        .any(|runtime_identity| runtime_identity != &state.config_identity)
    {
        return ConfigSnapshotStatus::RuntimeMismatch;
    }
    if primary.config_identity.is_none() {
        return ConfigSnapshotStatus::Unavailable;
    }
    ConfigSnapshotStatus::Consistent
}

fn container_environment_status(
    state: Option<&ContainerQueryStateSnapshot>,
    runtime: Option<&ContainerQueryRuntimeSnapshot>,
) -> EnvironmentStatus {
    let Some(runtime) = runtime else {
        return EnvironmentStatus::Unknown;
    };
    if runtime.containers.is_empty() {
        return EnvironmentStatus::Missing;
    }
    if state.is_some_and(|state| {
        !runtime.containers.iter().any(|container| {
            container.id.as_deref().is_some_and(|container_id| {
                container_ids_match(container_id, &state.primary_container_id)
            })
        })
    }) {
        return EnvironmentStatus::Partial;
    }
    aggregate_environment_status(
        runtime
            .containers
            .iter()
            .map(|container| container.run_state),
    )
}

fn container_health_status(runtime: Option<&ContainerQueryRuntimeSnapshot>) -> HealthStatus {
    let Some(runtime) = runtime else {
        return HealthStatus::Unknown;
    };
    aggregate_health_status(
        runtime
            .containers
            .iter()
            .map(|container| container.health_status),
    )
}

fn container_lifecycle_status(state: Option<&ContainerQueryStateSnapshot>) -> LifecycleStatus {
    aggregate_lifecycle_status(state.map(|state| state.lifecycle))
}

fn container_status_issues(
    config_snapshot_status: ConfigSnapshotStatus,
    environment_status: EnvironmentStatus,
    health_status: HealthStatus,
    lifecycle_status: LifecycleStatus,
    runtime: Option<&ContainerQueryRuntimeSnapshot>,
) -> Vec<ContainerQueryStatusIssue> {
    let mut issues = Vec::new();
    match config_snapshot_status {
        ConfigSnapshotStatus::Consistent => {}
        ConfigSnapshotStatus::RuntimeMismatch => issues.push(ContainerQueryStatusIssue {
            code: "runtime-mismatch",
            severity: StatusIssueSeverity::Warning,
            message: "Recorded state does not match managed runtime evidence.",
            action: Some("Run `decune rebuild` on the host."),
        }),
        ConfigSnapshotStatus::Unavailable => issues.push(ContainerQueryStatusIssue {
            code: "snapshot-unavailable",
            severity: StatusIssueSeverity::Info,
            message: "Config snapshot consistency could not be determined.",
            action: Some("Run `decune status` for this workspace on the host."),
        }),
    }
    match environment_status {
        EnvironmentStatus::Partial => issues.push(ContainerQueryStatusIssue {
            code: "partial-environment",
            severity: StatusIssueSeverity::Warning,
            message: "Only part of the recorded environment is present or running.",
            action: Some("Run `decune status` for this workspace on the host."),
        }),
        EnvironmentStatus::Missing => issues.push(ContainerQueryStatusIssue {
            code: "missing-environment",
            severity: StatusIssueSeverity::Warning,
            message: "No managed containers were found for this workspace.",
            action: Some("Run `decune rebuild` on the host."),
        }),
        EnvironmentStatus::Running
        | EnvironmentStatus::Stopped
        | EnvironmentStatus::NotCreated
        | EnvironmentStatus::Unknown => {}
    }
    if runtime.is_some_and(|runtime| {
        should_report_unhealthy(
            health_status,
            runtime
                .containers
                .iter()
                .map(|container| container.health_status),
        )
    }) {
        issues.push(ContainerQueryStatusIssue {
            code: "unhealthy-container",
            severity: StatusIssueSeverity::Error,
            message: "One or more managed containers are unhealthy.",
            action: Some("Inspect the affected containers on the host."),
        });
    }
    if lifecycle_status == LifecycleStatus::Incomplete {
        issues.push(ContainerQueryStatusIssue {
            code: "incomplete-lifecycle",
            severity: StatusIssueSeverity::Warning,
            message: "Creation lifecycle commands have not completed.",
            action: Some("Run `decune up` on the host to resume lifecycle execution."),
        });
    }
    issues
}

fn write_container_header(output: &mut String, status: &ContainerWorkspaceStatus) {
    _ = writeln!(output, "Workspace ID: {}", status.workspace_id);
    _ = writeln!(output, "Mode: {}", status.mode.as_str());
    output.push('\n');
}

fn write_container_summary(output: &mut String, status: &ContainerWorkspaceStatus) {
    output.push_str("Summary\n");
    _ = writeln!(output, "  Runtime: {}", status.environment_status.as_str());
    _ = writeln!(output, "  Health: {}", status.health_status.as_str());
    _ = writeln!(
        output,
        "  Config snapshot: {}",
        status.config_snapshot_status.as_str()
    );
    _ = writeln!(
        output,
        "  Live workspace: {}",
        status.live_workspace_status.as_str()
    );
    _ = writeln!(output, "  Containers: {}", status.containers.len());
    _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    output.push('\n');
}

fn write_recorded_state(output: &mut String, status: &ContainerWorkspaceStatus) {
    output.push_str("Recorded state\n");
    _ = writeln!(
        output,
        "  Created: {}",
        status.created_at.as_deref().unwrap_or("-")
    );
    _ = writeln!(
        output,
        "  Last started: {}",
        status.last_started_at.as_deref().unwrap_or("-")
    );
    _ = writeln!(
        output,
        "  Last used: {}",
        status.last_used_at.as_deref().unwrap_or("-")
    );
    output.push('\n');
}

fn write_container_issues(output: &mut String, status: &ContainerWorkspaceStatus) {
    if status.issues.is_empty() {
        return;
    }
    output.push_str("Issues\n");
    for issue in &status.issues {
        _ = writeln!(
            output,
            "  {} [{}]: {}",
            issue.code,
            issue.severity.as_str(),
            issue.message
        );
    }
    output.push('\n');
}

fn write_container_runtime(output: &mut String, status: &ContainerWorkspaceStatus) {
    output.push_str("Runtime\n");
    if status.containers.is_empty() {
        output.push_str("  No containers\n");
    } else {
        for container in &status.containers {
            let name = container
                .name
                .as_deref()
                .or(container.id.as_deref())
                .unwrap_or("<unknown>")
                .trim_start_matches('/');
            let service = container.service.as_deref().unwrap_or("-");
            _ = writeln!(
                output,
                "  {}  service={}  state={}  health={}",
                name,
                service,
                container.run_state.as_str(),
                container.health_status.as_str()
            );
        }
    }
    output.push('\n');
}

fn write_container_ports(output: &mut String, status: &ContainerWorkspaceStatus) {
    output.push_str("Ports\n");
    for line in render_ports_table(&status.ports, false).lines() {
        _ = writeln!(output, "  {line}");
    }
    output.push('\n');
}

fn write_container_resources(output: &mut String, status: &ContainerWorkspaceStatus) {
    output.push_str("Resources\n");
    _ = writeln!(output, "  Containers: {}", status.containers.len());
    _ = writeln!(output, "  Volumes: {}", status.volumes.len());
    let mut volume_names = status
        .volumes
        .iter()
        .filter_map(|volume| volume.name.as_deref())
        .collect::<Vec<_>>();
    volume_names.sort_unstable();
    for volume in volume_names {
        _ = writeln!(output, "  Volume: {volume}");
    }
    output.push('\n');
}

fn write_container_lifecycle(output: &mut String, status: &ContainerWorkspaceStatus) {
    if status.lifecycle_status != LifecycleStatus::Incomplete {
        return;
    }
    output.push_str("Lifecycle\n");
    if let Some(lifecycle) = status.lifecycle {
        _ = writeln!(
            output,
            "  onCreateCommand: {}",
            lifecycle_completion(
                lifecycle.is_command_completed(LifecycleCompletion::OnCreate),
                lifecycle.is_after_hook_completed(LifecycleCompletion::OnCreate)
            )
        );
        _ = writeln!(
            output,
            "  updateContentCommand: {}",
            lifecycle_completion(
                lifecycle.is_command_completed(LifecycleCompletion::UpdateContent),
                lifecycle.is_after_hook_completed(LifecycleCompletion::UpdateContent)
            )
        );
        _ = writeln!(
            output,
            "  postCreateCommand: {}",
            lifecycle_completion(
                lifecycle.is_command_completed(LifecycleCompletion::PostCreate),
                lifecycle.is_after_hook_completed(LifecycleCompletion::PostCreate)
            )
        );
    }
    output.push('\n');
}

fn write_host_actions(output: &mut String, status: &ContainerWorkspaceStatus) {
    let actions = status
        .issues
        .iter()
        .filter_map(|issue| issue.action.map(|action| (issue.code, action)))
        .collect::<Vec<_>>();
    if actions.is_empty() {
        return;
    }
    output.push_str("Action (run on host)\n");
    for (code, action) in actions {
        _ = writeln!(output, "  {code}: {action}");
    }
}

const fn lifecycle_completion(command: bool, after_hook: bool) -> &'static str {
    match (command, after_hook) {
        (true, true) => "complete",
        (true, false) => "after-hook-pending",
        (false, _) => "pending",
    }
}

fn finish_with_single_newline(mut output: String) -> String {
    while output.ends_with('\n') {
        _ = output.pop();
    }
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use crate::{
        ports::{PortInventoryEntry, PortUsageType},
        state::{
            CloneIsolationNetworkRuntimeState, CloneIsolationRuntimeState, WorkspaceModeSnapshot,
        },
    };

    use super::*;

    const WORKSPACE_ID: &str = "123456abcdef";
    const HOST_PATH: &str = "/host/private/workspace";
    const CONFIG_PATH: &str = "/host/private/workspace/.devcontainer/devcontainer.json";
    const RAW_CONFIG_HASH: &str = "raw-config-hash-secret-marker";
    const RAW_LABEL: &str = "raw-label-secret-marker";
    const SECRET: &str = "container-env-secret-marker";
    const MOUNT_SOURCE: &str = "/host/private/mount-source";

    #[test]
    fn consistent_snapshot_is_distinct_from_unchecked_live_workspace() {
        let snapshot = query_snapshot("hash", Some("hash"));

        let status = build_container_workspace_status(&snapshot);
        let output = render_container_workspace_status(&status);

        assert_eq!(
            status.config_snapshot_status,
            ConfigSnapshotStatus::Consistent
        );
        assert_eq!(
            status.live_workspace_status,
            LiveWorkspaceStatus::NotChecked
        );
        assert!(output.contains("Config snapshot: consistent"));
        assert!(output.contains("Live workspace: not checked"));
        assert!(!output.contains("Config: current"));
        assert!(!output.contains("needs-rebuild"));
        assert!(output.ends_with('\n'));
        assert!(!output.ends_with("\n\n"));
    }

    #[test]
    fn mismatch_and_missing_evidence_do_not_claim_live_config_comparison() {
        let mismatch = build_container_workspace_status(&query_snapshot(
            "recorded-hash",
            Some("runtime-hash"),
        ));
        let mut unavailable_snapshot = query_snapshot("recorded-hash", None);
        unavailable_snapshot.state = ContainerQueryStateEvidence::Missing;
        unavailable_snapshot.docker = ContainerQueryDockerEvidence::Unavailable;
        let unavailable = build_container_workspace_status(&unavailable_snapshot);
        let mut unreadable_snapshot = query_snapshot("recorded-hash", None);
        unreadable_snapshot.state = ContainerQueryStateEvidence::Unavailable;
        let unreadable = build_container_workspace_status(&unreadable_snapshot);

        assert_eq!(
            mismatch.config_snapshot_status,
            ConfigSnapshotStatus::RuntimeMismatch
        );
        assert_eq!(
            unavailable.config_snapshot_status,
            ConfigSnapshotStatus::Unavailable
        );
        assert_eq!(
            unreadable.config_snapshot_status,
            ConfigSnapshotStatus::Unavailable
        );
        let mismatch_output = render_container_workspace_status(&mismatch);
        assert!(mismatch_output.contains("Config snapshot: runtime-mismatch"));
        assert!(mismatch_output.contains("Action (run on host)"));
        assert!(mismatch_output.contains("Live workspace: not checked"));
    }

    #[test]
    fn mixed_health_without_unhealthy_does_not_report_an_issue() {
        let mut snapshot = query_snapshot("hash", Some("hash"));
        runtime_mut(&mut snapshot)
            .containers
            .push(container_evidence("sidecar-id", None, HealthStatus::None));

        let status = build_container_workspace_status(&snapshot);
        let output = render_container_workspace_status(&status);

        assert_eq!(status.health_status, HealthStatus::Mixed);
        assert_eq!(
            status.config_snapshot_status,
            ConfigSnapshotStatus::Consistent
        );
        assert!(
            status
                .issues
                .iter()
                .all(|issue| issue.code != "unhealthy-container"),
            "{:?}",
            status.issues
        );
        assert!(!output.contains("unhealthy-container"));
    }

    #[test]
    fn mixed_health_with_unhealthy_reports_an_error() {
        let mut snapshot = query_snapshot("hash", Some("hash"));
        runtime_mut(&mut snapshot)
            .containers
            .push(container_evidence(
                "sidecar-id",
                None,
                HealthStatus::Unhealthy,
            ));

        let status = build_container_workspace_status(&snapshot);
        let output = render_container_workspace_status(&status);
        let issue = status
            .issues
            .iter()
            .find(|issue| issue.code == "unhealthy-container")
            .unwrap();

        assert_eq!(status.health_status, HealthStatus::Mixed);
        assert_eq!(issue.severity, StatusIssueSeverity::Error);
        assert!(output.contains("unhealthy-container [error]"));
    }

    #[test]
    fn config_snapshot_checks_all_available_runtime_identities() {
        let mut matching = query_snapshot("hash", Some("hash"));
        runtime_mut(&mut matching)
            .containers
            .push(container_evidence(
                "sidecar-id",
                Some("hash"),
                HealthStatus::None,
            ));
        let matching_status = build_container_workspace_status(&matching);

        let mut mismatching = matching;
        runtime_mut(&mut mismatching).containers[1] =
            container_evidence("sidecar-id", Some("other-hash"), HealthStatus::None);
        let mismatching_status = build_container_workspace_status(&mismatching);

        assert_eq!(
            matching_status.config_snapshot_status,
            ConfigSnapshotStatus::Consistent
        );
        assert_eq!(
            mismatching_status.config_snapshot_status,
            ConfigSnapshotStatus::RuntimeMismatch
        );
    }

    #[test]
    fn missing_primary_is_a_runtime_mismatch_and_partial_environment() {
        let mut snapshot = query_snapshot("hash", Some("hash"));
        runtime_mut(&mut snapshot).containers[0].id = Some("replacement-id".to_owned());

        let status = build_container_workspace_status(&snapshot);

        assert_eq!(
            status.config_snapshot_status,
            ConfigSnapshotStatus::RuntimeMismatch
        );
        assert_eq!(status.environment_status, EnvironmentStatus::Partial);
        for code in ["runtime-mismatch", "partial-environment"] {
            assert!(
                status.issues.iter().any(|issue| issue.code == code),
                "{:?}",
                status.issues
            );
        }
    }

    #[test]
    fn missing_primary_identity_is_unavailable_without_a_known_mismatch() {
        let status = build_container_workspace_status(&query_snapshot("recorded-hash", None));

        assert_eq!(
            status.config_snapshot_status,
            ConfigSnapshotStatus::Unavailable
        );
    }

    #[test]
    fn known_mismatch_takes_precedence_over_missing_primary_identity() {
        let mut snapshot = query_snapshot("recorded-hash", None);
        runtime_mut(&mut snapshot)
            .containers
            .push(container_evidence(
                "sidecar-id",
                Some("other-hash"),
                HealthStatus::None,
            ));

        let status = build_container_workspace_status(&snapshot);

        assert_eq!(
            status.config_snapshot_status,
            ConfigSnapshotStatus::RuntimeMismatch
        );
    }

    #[test]
    fn missing_recorded_mode_degrades_to_unknown() {
        let mut state = workspace_state("hash");
        state.mode = WorkspaceModeSnapshot::Unknown;
        let snapshot = ContainerQuerySnapshot {
            workspace_id: WORKSPACE_ID.to_owned(),
            state: ContainerQueryStateEvidence::Available(ContainerQueryStateSnapshot::from_state(
                &state,
            )),
            docker: ContainerQueryDockerEvidence::Available(runtime_snapshot(Some("hash"))),
            ports: Vec::new(),
        };

        let status = build_container_workspace_status(&snapshot);

        assert_eq!(status.mode, WorkspaceMode::Unknown);
        assert!(render_container_workspace_status(&status).contains("Mode: unknown"));
    }

    #[test]
    fn sanitized_model_debug_serialize_and_output_exclude_forbidden_evidence() {
        let mut state = workspace_state(RAW_CONFIG_HASH);
        state.workspace = HOST_PATH.to_owned();
        state.config_file = Some(CONFIG_PATH.to_owned());
        state.image = SECRET.to_owned();
        state.compose_project_name = Some(RAW_LABEL.to_owned());
        state.clone_isolation = CloneIsolationRuntimeState {
            networks: vec![CloneIsolationNetworkRuntimeState {
                network: RAW_LABEL.to_owned(),
                requested_subnet: MOUNT_SOURCE.to_owned(),
                planned_subnet: SECRET.to_owned(),
                planned_gateway: Some(CONFIG_PATH.to_owned()),
                relocated: true,
            }],
        };
        let unsafe_port = PortInventoryEntry {
            workspace: Some(HOST_PATH.to_owned()),
            workspace_id: Some(WORKSPACE_ID.to_owned()),
            host_ip: "127.0.0.1".to_owned(),
            host_port: 3000,
            kind: PortUsageType::Forwarded,
            service: None,
            container_port: 3000,
            protocol: "tcp".to_owned(),
            source: "configured".to_owned(),
            port_entry_index: None,
            target: None,
            requested: None,
            planned: None,
            actual_bindings: None,
            requested_host_ip_kind: None,
            requested_host_ip: None,
            requested_host_port: None,
            planned_host_ip_kind: None,
            planned_host_ip: None,
            planned_host_port: None,
            relocated: None,
            label: Some("web".to_owned()),
        };
        let snapshot = ContainerQuerySnapshot {
            workspace_id: WORKSPACE_ID.to_owned(),
            state: ContainerQueryStateEvidence::Available(ContainerQueryStateSnapshot::from_state(
                &state,
            )),
            docker: ContainerQueryDockerEvidence::Available(runtime_snapshot(Some(
                RAW_CONFIG_HASH,
            ))),
            ports: vec![ContainerPortSnapshot::from(&unsafe_port)],
        };

        let status = build_container_workspace_status(&snapshot);
        let debug = format!("{snapshot:?}");
        let serialized = serde_json::to_string(&snapshot).unwrap();
        let status_json = serde_json::to_string(&status).unwrap();
        let output = render_container_workspace_status(&status);

        for forbidden in [
            HOST_PATH,
            CONFIG_PATH,
            RAW_CONFIG_HASH,
            RAW_LABEL,
            SECRET,
            MOUNT_SOURCE,
        ] {
            assert!(!debug.contains(forbidden), "{debug}");
            assert!(!serialized.contains(forbidden), "{serialized}");
            assert!(!status_json.contains(forbidden), "{status_json}");
            assert!(!output.contains(forbidden), "{output}");
        }
        assert!(!serialized.contains("\"workspace\""));
        assert!(!status_json.contains("\"workspace\""));
    }

    fn query_snapshot(
        recorded_config_hash: &str,
        runtime_config_hash: Option<&str>,
    ) -> ContainerQuerySnapshot {
        let state = workspace_state(recorded_config_hash);
        ContainerQuerySnapshot {
            workspace_id: WORKSPACE_ID.to_owned(),
            state: ContainerQueryStateEvidence::Available(ContainerQueryStateSnapshot::from_state(
                &state,
            )),
            docker: ContainerQueryDockerEvidence::Available(runtime_snapshot(runtime_config_hash)),
            ports: Vec::new(),
        }
    }

    fn runtime_snapshot(config_hash: Option<&str>) -> ContainerQueryRuntimeSnapshot {
        ContainerQueryRuntimeSnapshot {
            containers: vec![ContainerQueryContainerEvidence::new(
                Some("container-id".to_owned()),
                Some("/container-name".to_owned()),
                Some("app".to_owned()),
                RuntimeRunState::Running,
                HealthStatus::Healthy,
                config_hash,
            )],
            volumes: vec![ContainerQueryVolumeEvidence {
                name: Some("managed-volume".to_owned()),
            }],
        }
    }

    fn runtime_mut(snapshot: &mut ContainerQuerySnapshot) -> &mut ContainerQueryRuntimeSnapshot {
        match &mut snapshot.docker {
            ContainerQueryDockerEvidence::Available(runtime) => runtime,
            ContainerQueryDockerEvidence::Unavailable => {
                panic!("test snapshot must have runtime evidence")
            }
        }
    }

    fn container_evidence(
        id: &str,
        config_hash: Option<&str>,
        health_status: HealthStatus,
    ) -> ContainerQueryContainerEvidence {
        ContainerQueryContainerEvidence::new(
            Some(id.to_owned()),
            Some(format!("/{id}")),
            Some(id.to_owned()),
            RuntimeRunState::Running,
            health_status,
            config_hash,
        )
    }

    fn workspace_state(config_hash: &str) -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspace: "/workspace".to_owned(),
            mode: WorkspaceModeSnapshot::Compose,
            container_id: "container-id".to_owned(),
            image: "image".to_owned(),
            config_hash: config_hash.to_owned(),
            config_file: None,
            compose_project_name: None,
            published_ports: Vec::new(),
            clone_isolation: CloneIsolationRuntimeState::default(),
            created_at: "unix:1".to_owned(),
            last_started_at: "unix:2".to_owned(),
            last_used_at: Some("unix:3".to_owned()),
            lifecycle: LifecycleState::all_completed(),
        }
    }
}

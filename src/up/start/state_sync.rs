use std::{cell::RefCell, collections::BTreeSet};

use crate::{
    devcontainer::lifecycle::LifecycleRunPath,
    docker::{client::DockerClient, container::ContainerInspect, resource::COMPOSE_SERVICE_LABEL},
    runtime::compose_isolation::ComposeIsolationSubnetPlan,
    runtime::compose_ports::{
        ComposePublishedPortEndpoint, ComposePublishedPortHostIp, ComposePublishedPortPlan,
        ComposePublishedPortPlanEntry, ComposePublishedPortPlanningInput,
        compose_published_port_runtime_plan,
    },
    state::{
        self, CloneIsolationNetworkRuntimeState, CloneIsolationRuntimeState, ComposeRuntimeState,
        LifecycleState, PublishedPortActualBinding, PublishedPortEndpointState,
        PublishedPortHostIpKind, PublishedPortRuntimeState, PublishedPortRuntimeType,
        PublishedPortSource, PublishedPortTarget, StateContainerSnapshot, WorkspaceState,
    },
    up::types::{UpOutcome, UpPlan},
    workspace::Workspace,
};
use anyhow::{Result, bail};

use super::{CredentialRuntime, compose_port_protocol_name};

pub(in crate::up) struct StartedUpContainer {
    pub(in crate::up) client: DockerClient,
    pub(in crate::up) workspace: Workspace,
    pub(in crate::up) plan: UpPlan,
    pub(in crate::up) outcome: UpOutcome,
    pub(in crate::up) lifecycle_path: LifecycleRunPath,
    pub(in crate::up) state: RefCell<WorkspaceState>,
    _credentials: CredentialRuntime,
}

pub(super) fn started_up_container(
    client: DockerClient,
    workspace: Workspace,
    plan: UpPlan,
    outcome: UpOutcome,
    lifecycle_path: LifecycleRunPath,
    credentials: CredentialRuntime,
) -> Result<StartedUpContainer> {
    let state = sync_started_state(&workspace, &plan, &outcome, lifecycle_path)?;

    Ok(started_up_container_with_state(
        client,
        workspace,
        plan,
        outcome,
        lifecycle_path,
        credentials,
        state,
    ))
}

pub(super) const fn started_up_container_with_state(
    client: DockerClient,
    workspace: Workspace,
    plan: UpPlan,
    outcome: UpOutcome,
    lifecycle_path: LifecycleRunPath,
    credentials: CredentialRuntime,
    state: WorkspaceState,
) -> StartedUpContainer {
    StartedUpContainer {
        client,
        workspace,
        plan,
        outcome,
        lifecycle_path,
        state: RefCell::new(state),
        _credentials: credentials,
    }
}

fn sync_started_state(
    workspace: &Workspace,
    plan: &UpPlan,
    outcome: &UpOutcome,
    lifecycle_path: LifecycleRunPath,
) -> Result<WorkspaceState> {
    let container = state_container_snapshot(plan, outcome.container_id.clone());
    let compose_project_name = state_compose_project_name(plan);
    match lifecycle_path {
        LifecycleRunPath::New => state::sync_state_with_container_and_compose_project(
            workspace.paths().state_dir(),
            workspace.root(),
            container,
            compose_project_name,
            LifecycleState::default(),
        ),
        LifecycleRunPath::Started => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            write_reused_started_state(workspace, container, compose_project_name, &existing, true)
        }
        LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            write_reused_started_state(workspace, container, compose_project_name, &existing, false)
        }
    }
}

pub(super) fn reusable_lifecycle_state(
    workspace: &Workspace,
    container: &StateContainerSnapshot,
) -> Result<WorkspaceState> {
    let state_file = state::state_file_path(workspace.paths().state_dir());
    let existing = state::load_state_file(workspace.paths().state_dir())?;
    let Some(existing) =
        existing.filter(|state| state_matches_container_snapshot(state, container))
    else {
        bail!(
            "Cannot safely reuse existing dev container without matching lifecycle state: {}. Run decune rebuild to recreate it.",
            state_file.display()
        );
    };

    Ok(existing)
}

pub(super) fn write_reused_started_state(
    workspace: &Workspace,
    container: StateContainerSnapshot,
    compose_project_name: Option<String>,
    existing: &WorkspaceState,
    refresh_last_started_at: bool,
) -> Result<WorkspaceState> {
    state::write_reused_state_for_container(
        workspace.paths().state_dir(),
        workspace.root(),
        container,
        compose_project_name,
        existing,
        refresh_last_started_at,
    )
}

pub(super) struct ComposeStateSyncInput<'a> {
    pub(super) port_input: &'a ComposePublishedPortPlanningInput,
    pub(super) port_plan: &'a ComposePublishedPortPlan,
    pub(super) subnet_plan: &'a ComposeIsolationSubnetPlan,
}

pub(super) async fn sync_started_compose_state(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    outcome: &UpOutcome,
    lifecycle_path: LifecycleRunPath,
    runtime: ComposeStateSyncInput<'_>,
) -> Result<WorkspaceState> {
    let container = state_container_snapshot(plan, outcome.container_id.clone());
    let compose_project_name = state_compose_project_name(plan);
    let published_ports =
        compose_published_port_runtime_state(client, plan, runtime.port_input, runtime.port_plan)
            .await?;
    let compose_runtime = ComposeRuntimeState {
        published_ports,
        clone_isolation: clone_isolation_runtime_state(runtime.subnet_plan),
    };
    match lifecycle_path {
        LifecycleRunPath::New => state::sync_state_with_container_and_compose_runtime(
            workspace.paths().state_dir(),
            workspace.root(),
            container,
            compose_project_name,
            compose_runtime,
            LifecycleState::default(),
        ),
        LifecycleRunPath::Started => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            state::write_reused_state_for_container_with_compose_runtime(
                workspace.paths().state_dir(),
                workspace.root(),
                container,
                compose_project_name,
                compose_runtime,
                &existing,
                true,
            )
        }
        LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            state::write_reused_state_for_container_with_compose_runtime(
                workspace.paths().state_dir(),
                workspace.root(),
                container,
                compose_project_name,
                compose_runtime,
                &existing,
                false,
            )
        }
    }
}

fn clone_isolation_runtime_state(
    subnet_plan: &ComposeIsolationSubnetPlan,
) -> CloneIsolationRuntimeState {
    CloneIsolationRuntimeState {
        networks: subnet_plan
            .allocations
            .iter()
            .map(|allocation| CloneIsolationNetworkRuntimeState {
                network: allocation.network.clone(),
                requested_subnet: allocation.requested_subnet.clone(),
                planned_subnet: allocation.planned_subnet.clone(),
                planned_gateway: allocation.planned_gateway.clone(),
                relocated: allocation.relocated,
            })
            .collect(),
    }
}

async fn compose_published_port_runtime_state(
    client: &DockerClient,
    plan: &UpPlan,
    port_input: &ComposePublishedPortPlanningInput,
    port_plan: &ComposePublishedPortPlan,
) -> Result<Vec<PublishedPortRuntimeState>> {
    let runtime_plan = compose_published_port_runtime_plan(port_input, port_plan)
        .map_err(crate::runtime::compose_ports::ComposePublishedPortDiagnostic::from_plan_error)?;
    if runtime_plan.entries.is_empty() {
        return Ok(Vec::new());
    }
    let containers = match plan
        .compose_project
        .as_ref()
        .map(|project| project.project_name().to_owned())
    {
        Some(project_name) => client
            .cli()
            .list_compose_project_container_inspects_by_project(&project_name)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    Ok(runtime_plan
        .entries
        .into_iter()
        .map(|entry| published_port_runtime_state_for_entry(&containers, entry))
        .collect())
}

fn published_port_runtime_state_for_entry(
    containers: &[ContainerInspect],
    entry: ComposePublishedPortPlanEntry,
) -> PublishedPortRuntimeState {
    let actual = actual_bindings_for_compose_published_port(containers, &entry);
    let planned = planned_endpoint_for_runtime_state(&entry, &actual);
    let actual_bindings = actual
        .into_iter()
        .filter(|binding| binding.host_port == planned.host_port)
        .collect::<Vec<_>>();
    let relocated = entry.requested != planned;
    PublishedPortRuntimeState {
        source: PublishedPortSource::Compose,
        kind: PublishedPortRuntimeType::Published,
        service: entry.service,
        port_entry_index: entry.port_entry_index,
        target: PublishedPortTarget {
            port: entry.target_port,
            protocol: compose_port_protocol_name(&entry.protocol).to_owned(),
        },
        requested: published_port_endpoint_state(&entry.requested),
        planned: published_port_endpoint_state(&planned),
        actual_bindings,
        relocated,
    }
}

fn planned_endpoint_for_runtime_state(
    entry: &crate::runtime::compose_ports::ComposePublishedPortPlanEntry,
    actual: &[PublishedPortActualBinding],
) -> ComposePublishedPortEndpoint {
    if actual
        .iter()
        .any(|binding| binding.host_port == entry.planned.host_port)
    {
        return entry.planned.clone();
    }
    let distinct_ports = actual
        .iter()
        .map(|binding| binding.host_port)
        .collect::<BTreeSet<_>>();
    if distinct_ports.len() == 1
        && let Some(host_port) = distinct_ports.iter().next()
    {
        let mut planned = entry.planned.clone();
        planned.host_port = *host_port;
        return planned;
    }
    entry.planned.clone()
}

fn actual_bindings_for_compose_published_port(
    containers: &[ContainerInspect],
    entry: &crate::runtime::compose_ports::ComposePublishedPortPlanEntry,
) -> Vec<PublishedPortActualBinding> {
    let port_key = format!(
        "{}/{}",
        entry.target_port,
        compose_port_protocol_name(&entry.protocol)
    );
    let mut bindings = BTreeSet::<(String, u16)>::new();
    for container in containers {
        if !container_is_running(container) {
            continue;
        }
        let Some(labels) = container
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
        else {
            continue;
        };
        if labels
            .get(COMPOSE_SERVICE_LABEL)
            .is_none_or(|service| service != &entry.service)
        {
            continue;
        }
        let Some(ports) = container
            .network_settings
            .as_ref()
            .and_then(|settings| settings.ports.as_ref())
        else {
            continue;
        };
        let Some(Some(port_bindings)) = ports.get(&port_key) else {
            continue;
        };
        for binding in port_bindings {
            let Some(host_port) = binding
                .host_port
                .as_deref()
                .and_then(|host_port| host_port.parse::<u16>().ok())
            else {
                continue;
            };
            let host_ip = binding
                .host_ip
                .as_deref()
                .filter(|host_ip| !host_ip.trim().is_empty())
                .unwrap_or("0.0.0.0")
                .to_owned();
            bindings.insert((host_ip, host_port));
        }
    }

    bindings
        .into_iter()
        .map(|(host_ip, host_port)| PublishedPortActualBinding { host_ip, host_port })
        .collect()
}

fn container_is_running(container: &ContainerInspect) -> bool {
    container.state.as_ref().is_some_and(|state| {
        state.running == Some(true) || state.status.as_deref() == Some("running")
    })
}

fn published_port_endpoint_state(
    endpoint: &ComposePublishedPortEndpoint,
) -> PublishedPortEndpointState {
    PublishedPortEndpointState {
        ip_kind: match &endpoint.host_ip {
            ComposePublishedPortHostIp::Omitted => PublishedPortHostIpKind::Omitted,
            ComposePublishedPortHostIp::Explicit(_) => PublishedPortHostIpKind::Explicit,
        },
        ip_value: match &endpoint.host_ip {
            ComposePublishedPortHostIp::Omitted => None,
            ComposePublishedPortHostIp::Explicit(value) => Some(value.clone()),
        },
        host_port: endpoint.host_port,
    }
}

pub(super) fn state_compose_project_name(plan: &UpPlan) -> Option<String> {
    plan.compose_project
        .as_ref()
        .map(|project| project.project_name().to_owned())
}

pub(super) fn state_container_snapshot(
    plan: &UpPlan,
    container_id: String,
) -> StateContainerSnapshot {
    StateContainerSnapshot {
        container_id,
        image: plan.image.clone(),
        config_hash: plan.resources.config_hash.clone(),
        config_file: plan
            .resources
            .labels
            .get("devcontainer.config_file")
            .cloned(),
    }
}

fn state_matches_container_snapshot(
    state: &WorkspaceState,
    container: &StateContainerSnapshot,
) -> bool {
    state::container_ids_match(&state.container_id, &container.container_id)
        && state.config_hash == container.config_hash
}

#[cfg(test)]
mod tests {
    use super::super::test_support::generated_override_test_plan;
    use super::*;
    use crate::runtime::compose_ports::{
        ComposePortProtocol, ComposePublishedPortAllocationReason,
        ComposePublishedPortPlanEntryType, ComposePublishedPortPlanSource,
        ComposePublishedPortPlannedEndpointProbe,
    };

    #[test]
    fn state_snapshot_records_final_image_tag_for_compose_plan() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.image = "decune/project-abc123:config-hash".to_owned();
        plan.base_image = "example/app:dev".to_owned();
        plan.resources.config_hash = "config-hash".to_owned();

        let snapshot = state_container_snapshot(&plan, "container-id".to_owned());

        assert_eq!(snapshot.image, "decune/project-abc123:config-hash");
        assert_eq!(snapshot.config_hash, "config-hash");
    }

    #[test]
    fn host_ip_only_published_port_change_is_relocated_in_runtime_state() {
        let state = published_port_runtime_state_for_entry(
            &[],
            ComposePublishedPortPlanEntry {
                service: "app".to_owned(),
                port_entry_index: 0,
                source: ComposePublishedPortPlanSource::Compose,
                kind: ComposePublishedPortPlanEntryType::Published,
                target_port: 3000,
                protocol: ComposePortProtocol::Tcp,
                requested: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Explicit("127.0.0.1".to_owned()),
                    host_port: 3000,
                },
                planned: ComposePublishedPortEndpoint {
                    host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                    host_port: 3000,
                },
                planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
                relocated: true,
                allocation_reason: ComposePublishedPortAllocationReason::Mapping,
            },
        );

        assert!(state.relocated);
        assert_eq!(state.requested.ip_value.as_deref(), Some("127.0.0.1"));
        assert_eq!(state.requested.host_port, 3000);
        assert_eq!(state.planned.ip_value.as_deref(), Some("0.0.0.0"));
        assert_eq!(state.planned.host_port, 3000);
    }
}

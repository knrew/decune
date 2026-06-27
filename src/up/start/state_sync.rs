use super::*;

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

pub(super) fn started_up_container_with_state(
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
            write_reused_started_state(workspace, container, compose_project_name, existing, true)
        }
        LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            write_reused_started_state(workspace, container, compose_project_name, existing, false)
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
    existing: WorkspaceState,
    refresh_last_started_at: bool,
) -> Result<WorkspaceState> {
    state::write_reused_state_for_container(
        workspace.paths().state_dir(),
        workspace.root(),
        container,
        compose_project_name,
        &existing,
        refresh_last_started_at,
    )
}

pub(super) async fn sync_started_compose_state(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    outcome: &UpOutcome,
    lifecycle_path: LifecycleRunPath,
    port_input: &ComposePublishedPortPlanningInput,
    port_plan: &ComposePublishedPortPlan,
) -> Result<WorkspaceState> {
    let container = state_container_snapshot(plan, outcome.container_id.clone());
    let compose_project_name = state_compose_project_name(plan);
    let published_ports =
        compose_published_port_runtime_state(client, plan, port_input, port_plan).await;
    match lifecycle_path {
        LifecycleRunPath::New => {
            state::sync_state_with_container_and_compose_project_and_published_ports(
                workspace.paths().state_dir(),
                workspace.root(),
                container,
                compose_project_name,
                published_ports,
                LifecycleState::default(),
            )
        }
        LifecycleRunPath::Started => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            state::write_reused_state_for_container_with_published_ports(
                workspace.paths().state_dir(),
                workspace.root(),
                container,
                compose_project_name,
                published_ports,
                &existing,
                true,
            )
        }
        LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            state::write_reused_state_for_container_with_published_ports(
                workspace.paths().state_dir(),
                workspace.root(),
                container,
                compose_project_name,
                published_ports,
                &existing,
                false,
            )
        }
    }
}

async fn compose_published_port_runtime_state(
    client: &DockerClient,
    plan: &UpPlan,
    port_input: &ComposePublishedPortPlanningInput,
    port_plan: &ComposePublishedPortPlan,
) -> Vec<PublishedPortRuntimeState> {
    let runtime_plan = compose_published_port_runtime_plan(port_input, port_plan);
    if runtime_plan.entries.is_empty() {
        return Vec::new();
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

    runtime_plan
        .entries
        .into_iter()
        .map(|entry| {
            let actual = actual_bindings_for_compose_published_port(&containers, &entry);
            let planned = planned_endpoint_for_runtime_state(&entry, &actual);
            let actual_bindings = actual
                .into_iter()
                .filter(|binding| binding.host_port == planned.host_port)
                .collect::<Vec<_>>();
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
                relocated: entry.requested.host_port != planned.host_port,
                planned: published_port_endpoint_state(&planned),
                actual_bindings,
            }
        })
        .collect()
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
    if distinct_ports.len() == 1 {
        let mut planned = entry.planned.clone();
        planned.host_port = *distinct_ports
            .iter()
            .next()
            .expect("distinct port set is non-empty");
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
            .get("com.docker.compose.service")
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
        host_ip_kind: match endpoint.host_ip_kind {
            ComposePublishedPortHostIpKind::Omitted => PublishedPortHostIpKind::Omitted,
            ComposePublishedPortHostIpKind::Explicit => PublishedPortHostIpKind::Explicit,
        },
        host_ip_value: endpoint.host_ip_value.clone(),
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
}
